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

use crate::builtins::{call_pure_builtin, is_pure_call};
use crate::policy::Policy;

/// Output sink used by `io.print`. Tests inject a buffer; production prints
/// to stdout.
pub trait IoSink: Send {
    fn print_line(&mut self, s: &str);
}

pub struct StdoutSink;
impl IoSink for StdoutSink {
    fn print_line(&mut self, s: &str) {
        use std::io::Write;
        println!("{s}");
        let _ = std::io::stdout().flush();
    }
}

#[derive(Default)]
pub struct CapturedSink { pub lines: Vec<String> }
impl IoSink for CapturedSink {
    fn print_line(&mut self, s: &str) { self.lines.push(s.to_string()); }
}

/// `agent.cloud_stream` registry: per-handle producer iterators
/// keyed by opaque handle id (#305 slice 3).
pub type StreamRegistry =
    std::collections::HashMap<String, Box<dyn Iterator<Item = String> + Send>>;

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
    /// Stream registry for `agent.cloud_stream` / `stream.next` /
    /// `stream.collect` (#305 slice 3). Keyed by an opaque handle
    /// id; values are the producer iterators. Wrapped in
    /// `Arc<Mutex<…>>` so par_map workers can share the same
    /// stream pool (when slice-2's per-worker handler split chains
    /// the registry through).
    pub streams: Arc<std::sync::Mutex<StreamRegistry>>,
    /// Monotonic counter for handing out fresh stream handle ids.
    pub next_stream_id: Arc<std::sync::atomic::AtomicU64>,
    /// Stack of per-request arenas (#463 scaffolding). One entry
    /// per active request scope; `net.serve_fn`'s request loop
    /// pushes on entry, pops on exit. Today nothing reads from the
    /// arenas — they're scaffolding for the Value-rep follow-on
    /// that routes `MakeRecord` / `MakeList` allocations into the
    /// active arena. See `crates/lex-runtime/src/arena.rs`.
    ///
    /// Held by value (not Arc) so worker-clone handlers
    /// (`spawn_for_worker`) get a fresh empty stack rather than
    /// sharing the parent's arenas — worker-thread allocations
    /// have a different lifetime than the request that spawned
    /// them.
    arena_stack: Vec<(u64, crate::arena::Arena)>,
    /// Monotonic counter for the scope ids handed out by
    /// `enter_request_scope`. `enter` returns a fresh id; `exit`
    /// finds and removes the matching entry. Plain `u64`, not
    /// shared — each handler instance has its own counter.
    next_scope_id: u64,
    /// Arguments passed after `--` in `lex run <file> -- [args...]`.
    /// Returned by `io.argv()` so Lex `main` functions can read CLI flags.
    pub program_args: Vec<String>,
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
            streams: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            next_stream_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            arena_stack: Vec::new(),
            next_scope_id: 1,
            program_args: Vec::new(),
        }
    }

    /// Read-only access to the currently-active request arena, if
    /// any. `None` outside a request scope. The follow-on slice
    /// that routes `Value` allocations consults this from the VM
    /// path; today it has no callers in tree but is exercised in
    /// tests.
    pub fn active_arena(&self) -> Option<&crate::arena::Arena> {
        self.arena_stack.last().map(|(_, a)| a)
    }

    /// Test-only: depth of the arena stack. Lets tests confirm the
    /// `net.serve_fn` request loop pushes/pops symmetrically.
    pub fn arena_stack_depth(&self) -> usize {
        self.arena_stack.len()
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

    pub fn with_program_args(mut self, args: Vec<String>) -> Self {
        self.program_args = args; self
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
                        "log.set_level: unknown level `{s}`; expected debug|info|warn|error").into()))),
                }
            }
            "set_format" => {
                let s = expect_str(args.first())?;
                let fmt = match s {
                    "text" => LogFormat::Text,
                    "json" => LogFormat::Json,
                    other => return Ok(err(Value::Str(format!(
                        "log.set_format: unknown format `{other}`; expected text|json").into()))),
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
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::OpenOptions::new()
                    .create(true).append(true).open(path)
                {
                    Ok(f) => {
                        log_state().lock().unwrap().sink = LogSink::File(std::sync::Arc::new(Mutex::new(f)));
                        Ok(ok(Value::Unit))
                    }
                    Err(e) => Ok(err(Value::Str(format!("log.set_sink `{path}`: {e}").into()))),
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
                    Value::Str(s) => Ok(s.to_string()),
                    other => Err(format!("process.spawn: arg must be Str, got {other:?}")),
                }).collect();
                let str_args = str_args?;
                let opts = match args.get(2) {
                    Some(Value::Record { fields: r, .. }) => r.clone(),
                    _ => return Err("process.spawn: missing or invalid opts record".into()),
                };

                // Allow-list check, mirroring process.run below.
                if !self.policy.allow_proc.is_empty() {
                    let basename = std::path::Path::new(&cmd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&cmd);
                    if !self.policy.allow_proc.iter().any(|a| a == basename) {
                        return Ok(err(Value::Str(format!(
                            "process.spawn: `{cmd}` not in --allow-proc {:?}",
                            self.policy.allow_proc
                        ).into())));
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
                    Err(e) => return Ok(err(Value::Str(format!("process.spawn `{cmd}`: {e}").into()))),
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
                Ok(Value::record_dynamic(rec))
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
                    Err(e) => Ok(err(Value::Str(format!("process.kill: {e}").into()))),
                }
            }
            "run" => {
                let cmd = expect_str(args.first())?.to_string();
                let raw_args = match args.get(1) {
                    Some(Value::List(items)) => items.clone(),
                    _ => return Err("process.run: args must be List[Str]".into()),
                };
                let str_args: Result<Vec<String>, String> = raw_args.iter().map(|v| match v {
                    Value::Str(s) => Ok(s.to_string()),
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
                        ).into())));
                    }
                }
                match std::process::Command::new(&cmd).args(&str_args).output() {
                    Ok(o) => {
                        let mut rec = indexmap::IndexMap::new();
                        rec.insert("stdout".into(), Value::Str(
                            String::from_utf8_lossy(&o.stdout).into_owned().into()));
                        rec.insert("stderr".into(), Value::Str(
                            String::from_utf8_lossy(&o.stderr).into_owned().into()));
                        rec.insert("exit_code".into(), Value::Int(
                            o.status.code().unwrap_or(-1) as i64));
                        Ok(ok(Value::record_dynamic(rec)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("process.run `{cmd}`: {e}").into()))),
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
                Ok(some(Value::Str(line.into())))
            }
            Err(e) => Err(format!("process.read_*_line: {e}")),
        }
    }

    fn dispatch_fs(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "exists" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                Ok(Value::Bool(std::path::Path::new(&path).exists()))
            }
            "is_file" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                Ok(Value::Bool(std::path::Path::new(&path).is_file()))
            }
            "is_dir" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                Ok(Value::Bool(std::path::Path::new(&path).is_dir()))
            }
            "stat" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
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
                        Ok(ok(Value::record_dynamic(rec)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("fs.stat `{path}`: {e}").into()))),
                }
            }
            "list_dir" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::read_dir(&path) {
                    Ok(rd) => {
                        let mut entries: Vec<Value> = Vec::new();
                        for ent in rd {
                            match ent {
                                Ok(e) => {
                                    let p = e.path();
                                    entries.push(Value::Str(p.to_string_lossy().into_owned().into()));
                                }
                                Err(e) => return Ok(err(Value::Str(format!("fs.list_dir: {e}").into()))),
                            }
                        }
                        Ok(ok(Value::List(entries.into())))
                    }
                    Err(e) => Ok(err(Value::Str(format!("fs.list_dir `{path}`: {e}").into()))),
                }
            }
            "walk" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                let mut paths: Vec<Value> = Vec::new();
                for ent in walkdir::WalkDir::new(&path) {
                    match ent {
                        Ok(e) => paths.push(Value::Str(
                            e.path().to_string_lossy().into_owned().into())),
                        Err(e) => return Ok(err(Value::Str(format!("fs.walk: {e}").into()))),
                    }
                }
                Ok(ok(Value::List(paths.into())))
            }
            "glob" => {
                let pattern = expect_str(args.first())?.to_string();
                // Glob patterns can't be path-scoped at parse time
                // (`**/*.rs` doesn't pin a directory); we filter the
                // per-result paths after expansion against
                // `--allow-fs-read`.
                let entries = match glob::glob(&pattern) {
                    Ok(e) => e,
                    Err(e) => return Ok(err(Value::Str(format!("fs.glob: {e}").into()))),
                };
                let mut paths: Vec<Value> = Vec::new();
                for ent in entries {
                    match ent {
                        Ok(p) => {
                            let s = p.to_string_lossy().into_owned();
                            if self.policy.allow_fs_read.is_empty()
                                || self.policy.allow_fs_read.iter().any(|root| p.starts_with(root))
                            {
                                paths.push(Value::Str(s.into()));
                            }
                        }
                        Err(e) => return Ok(err(Value::Str(format!("fs.glob: {e}").into()))),
                    }
                }
                Ok(ok(Value::List(paths.into())))
            }
            "mkdir_p" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_write_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::create_dir_all(&path) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.mkdir_p `{path}`: {e}").into()))),
                }
            }
            "remove" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_write_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                let p = std::path::Path::new(&path);
                let result = if p.is_dir() {
                    std::fs::remove_dir_all(p)
                } else {
                    std::fs::remove_file(p)
                };
                match result {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.remove `{path}`: {e}").into()))),
                }
            }
            "copy" => {
                let src = expect_str(args.first())?.to_string();
                let dst = expect_str(args.get(1))?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&src) {
                    return Ok(err(Value::Str(e.into())));
                }
                if let Err(e) = self.ensure_fs_write_path(&dst) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::copy(&src, &dst) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.copy {src} -> {dst}: {e}").into()))),
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
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("redis://"))
        .or_else(|| url.strip_prefix("rediss://"))
        // `@user:pass@host:port` — strip auth prefix if present
        .map(|r| r.split_once('@').map(|(_, after)| after).unwrap_or(r))?;
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
    /// Push a fresh per-request arena onto the stack (#463
    /// scaffolding). Returns the scope id; pair with
    /// `exit_request_scope(id)` to drop it.
    fn enter_request_scope(&mut self) -> u64 {
        let id = self.next_scope_id;
        self.next_scope_id = self.next_scope_id.wrapping_add(1);
        self.arena_stack.push((id, crate::arena::Arena::new()));
        id
    }

    /// Drop the arena associated with `scope_id`. Mismatched pairs
    /// (exit called with a scope id we don't recognize, or out-of-
    /// order exit) are tolerated as no-ops rather than panicking —
    /// runtime layer should pair them strictly but a stray exit
    /// shouldn't crash a live server.
    fn exit_request_scope(&mut self, scope_id: u64) {
        if let Some(pos) = self.arena_stack.iter().position(|(id, _)| *id == scope_id) {
            // Drop this entry and any later entries that escaped
            // pairing (out-of-order exit). Order matters: pop in
            // reverse so the most recent arena drops first, then
            // its predecessor, etc.
            self.arena_stack.truncate(pos);
        }
    }

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
        if is_pure_call(kind, op) {
            return call_pure_builtin(kind, op, args);
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
            // LEX_TEST_NOW (Unix seconds) pins the clock for deterministic tests (#350).
            if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                if let Ok(secs) = s.trim().parse::<i64>() {
                    return Ok(Value::Int(secs.saturating_mul(1_000_000_000)));
                }
            }
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
            use rand::{rngs::SysRng, TryRng};
            let mut buf = vec![0u8; n as usize];
            SysRng.try_fill_bytes(&mut buf)
                .map_err(|e| format!("crypto.random: OS RNG: {e}"))?;
            return Ok(Value::Bytes(buf));
        }
        // crypto.random_str_hex(n) — N random bytes rendered as 2N
        // lowercase hex chars (#382). The most common token-mint
        // pattern (session ids, OAuth `state`, CSRF, request ids).
        // Same `[random]` gate as `crypto.random`.
        if kind == "crypto" && op == "random_str_hex" {
            self.ensure_kind_allowed("random")?;
            let n = expect_int(args.first())?;
            if !(0..=1_048_576).contains(&n) {
                return Err("crypto.random_str_hex: n must be in 0..=1048576".into());
            }
            use rand::{rngs::SysRng, TryRng};
            let mut buf = vec![0u8; n as usize];
            SysRng.try_fill_bytes(&mut buf)
                .map_err(|e| format!("crypto.random_str_hex: OS RNG: {e}"))?;
            return Ok(Value::Str(hex::encode(&buf).into()));
        }
        // crypto.p256_generate() — mint a fresh P-256 (ES256) secret
        // key from the OS RNG (#651). Returns the 32-byte scalar as
        // `Ok(Bytes)`. Same `[random]` gate as `crypto.random`: key
        // minting stays visible to `lex audit --effect random`.
        //
        // We sample 32 bytes and let `SigningKey::from_slice` reject
        // the (vanishingly rare, ~2^-32) out-of-range scalar rather
        // than pulling in p256's own `rand_core` — that crate is on a
        // different `rand_core` major than the workspace `rand`, so
        // bridging RNG traits here would mean an extra dependency for
        // no behavioural gain. Retry a handful of times so a one-in-
        // four-billion miss never surfaces as a spurious `Err`.
        if kind == "crypto" && op == "p256_generate" {
            self.ensure_kind_allowed("random")?;
            use p256::ecdsa::SigningKey;
            use rand::{rngs::SysRng, TryRng};
            for _ in 0..16 {
                let mut buf = [0u8; 32];
                SysRng.try_fill_bytes(&mut buf)
                    .map_err(|e| format!("crypto.p256_generate: OS RNG: {e}"))?;
                if let Ok(sk) = SigningKey::from_slice(&buf) {
                    return Ok(ok(Value::Bytes(sk.to_bytes().to_vec())));
                }
            }
            return Ok(err(Value::Str(
                "crypto.p256_generate: failed to sample a valid scalar".into())));
        }
        // crypto.secp256k1_generate() — mint a fresh secp256k1 secret key
        // from the OS RNG (#655) for EVM / EIP-712 / x402 signing. Returns
        // the 32-byte scalar as `Ok(Bytes)`. Same `[random]` gate and
        // sample-and-reject loop as `p256_generate` (the curve order is
        // close enough to 2^256 that a miss is ~2^-128, but the loop
        // keeps the contract identical).
        if kind == "crypto" && op == "secp256k1_generate" {
            self.ensure_kind_allowed("random")?;
            use k256::ecdsa::SigningKey;
            use rand::{rngs::SysRng, TryRng};
            for _ in 0..16 {
                let mut buf = [0u8; 32];
                SysRng.try_fill_bytes(&mut buf)
                    .map_err(|e| format!("crypto.secp256k1_generate: OS RNG: {e}"))?;
                if let Ok(sk) = SigningKey::from_slice(&buf) {
                    return Ok(ok(Value::Bytes(sk.to_bytes().to_vec())));
                }
            }
            return Ok(err(Value::Str(
                "crypto.secp256k1_generate: failed to sample a valid scalar".into())));
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
                "cloud_stream"   => "llm_cloud",
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
                "cloud_stream"   => Ok(self.dispatch_cloud_stream(args)),
                _ => Ok(ok(Value::Str(format!("<{effect_kind} stub>").into()))),
            };
        }
        if kind == "stream" {
            // #305 slice 3: consumer-side stream operations. Each
            // op resolves the opaque handle in the parent handler's
            // stream registry and pulls one or all items. The
            // `stream` effect must be granted by policy; default
            // policies for agent runs grant it alongside the
            // producer effect (e.g. `llm_cloud`).
            self.ensure_kind_allowed("stream")?;
            return match op {
                "next"    => Ok(self.dispatch_stream_next(args)),
                "collect" => Ok(self.dispatch_stream_collect(args)),
                other => Err(format!("unsupported stream.{other}")),
            };
        }
        if kind == "http" && matches!(op, "send" | "get" | "post" | "stream_lines") {
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
                "stream_lines" => {
                    let url = expect_str(args.first())?.to_string();
                    let headers_val = args.get(1).cloned().unwrap_or(Value::Map(Default::default()));
                    let body = expect_str(args.get(2))?.to_string();
                    self.ensure_host_allowed(&url)?;
                    Ok(http_stream_lines_impl(self, &url, &headers_val, &body))
                }
                _ => unreachable!(),
            };
        }
        // `arrow.read_csv` declares `[fs_read]`, not `[arrow]` — its effect
        // string in the type system is `fs_read`. Intercept before the
        // generic `ensure_kind_allowed(kind)` below so the policy check
        // looks at `fs_read` rather than `arrow`. Same pattern as
        // `http.{send,get,post}` mapping to `[net]` above.
        if kind == "arrow" && op == "read_csv" {
            self.ensure_kind_allowed("fs_read")?;
            let path = expect_str(args.first())?.to_string();
            let resolved = self.resolve_read_path(&path);
            if !self.policy.allow_fs_read.is_empty()
                && !self.policy.allow_fs_read.iter().any(|a| resolved.starts_with(a))
            {
                return Err(format!("arrow.read_csv: `{path}` outside --allow-fs-read"));
            }
            return match crate::arrow::read_csv_at(&resolved) {
                Ok(v)  => Ok(ok(v)),
                Err(e) => Ok(err(Value::Str(e.into()))),
            };
        }
        // `arrow.read_parquet` and `arrow.read_parquet_cols` are the
        // Parquet siblings of `read_csv`. Same `[fs_read]` effect, same
        // path-scope check. `_cols` takes an extra `List[Str]` argument.
        if kind == "arrow" && (op == "read_parquet" || op == "read_parquet_cols") {
            self.ensure_kind_allowed("fs_read")?;
            let path = expect_str(args.first())?.to_string();
            let resolved = self.resolve_read_path(&path);
            if !self.policy.allow_fs_read.is_empty()
                && !self.policy.allow_fs_read.iter().any(|a| resolved.starts_with(a))
            {
                return Err(format!("arrow.{op}: `{path}` outside --allow-fs-read"));
            }
            let r = if op == "read_parquet" {
                crate::arrow::read_parquet_at(&resolved)
            } else {
                let cols = match args.get(1) {
                    Some(Value::List(items)) => {
                        let mut out = Vec::with_capacity(items.len());
                        for v in items.iter() {
                            match v {
                                Value::Str(s) => out.push(s.to_string()),
                                other => return Err(format!(
                                    "arrow.read_parquet_cols: column name not Str: {other:?}")),
                            }
                        }
                        out
                    }
                    other => return Err(format!(
                        "arrow.read_parquet_cols: expected List[Str], got {other:?}")),
                };
                crate::arrow::read_parquet_cols_at(&resolved, &cols)
            };
            return match r {
                Ok(v) => Ok(ok(v)),
                Err(e) => Ok(err(Value::Str(e.into()))),
            };
        }
        // `arrow.write_parquet` and `arrow.write_csv` declare `[fs_write]`.
        // Path scope uses `--allow-fs-write` (symmetric with `io.write`).
        if kind == "arrow" && (op == "write_parquet" || op == "write_csv") {
            self.ensure_kind_allowed("fs_write")?;
            let table_v = args.first().cloned().unwrap_or(Value::Unit);
            let rb = match &table_v {
                Value::ArrowTable(t) => Arc::clone(t),
                other => return Err(format!("arrow.{op}: first arg must be arrow.Table, got {other:?}")),
            };
            let path = expect_str(args.get(1))?.to_string();
            if let Err(e) = self.ensure_fs_write_path(&path) {
                return Ok(err(Value::Str(format!("arrow.{op}: {e}").into())));
            }
            let r = if op == "write_parquet" {
                crate::arrow::write_parquet_at(&rb, std::path::Path::new(&path))
            } else {
                crate::arrow::write_csv_at(&rb, std::path::Path::new(&path))
            };
            return match r {
                Ok(_)  => Ok(ok(Value::Unit)),
                Err(e) => Ok(err(Value::Str(e.into()))),
            };
        }
        // `net.default_opts()` is a pure record constructor — typed
        // with `EffectSet::empty()` in builtins.rs. Bypass the generic
        // `ensure_kind_allowed("net")` gate so callers don't need to
        // declare `[net]` just to build a ServeOpts literal default.
        if kind == "net" && op == "default_opts" {
            return Ok(ServeOpts::lex_defaults().to_value());
        }
        // `tls.*` (#496) — TlsConfig constructors map to different
        // effect kinds than the namespace name suggests:
        //   `tls.from_pem_files` :: [fs_read]   (reads cert + key PEM)
        //   `tls.self_signed`    :: pure        (rcgen, in-memory)
        // Intercept before the generic `ensure_kind_allowed("tls")`
        // gate so policy can check the *real* effect. Same pattern
        // as the `http.{send,get,post}` arms above.
        if kind == "tls" {
            return match op {
                "from_pem_files" => {
                    self.ensure_kind_allowed("fs_read")?;
                    dispatch_tls_from_pem_files(self, args)
                }
                "self_signed" => dispatch_tls_self_signed(args),
                other => Err(format!("unsupported tls.{other}")),
            };
        }
        // `std.redis` ops all carry `[net]` in their declared effect sets,
        // not `[redis]`. Gate on `net` here and skip the generic kind-check
        // below, matching the `std.http` precedent.
        if kind == "redis" {
            self.ensure_kind_allowed("net")?;
        } else if kind == "rand" {
            // `std.rand.int_in` draws from the OS RNG → `[random]` effect,
            // the same gate as `crypto.random` (#677). No separate `rand`
            // effect grant exists.
            self.ensure_kind_allowed("random")?;
        } else {
            self.ensure_kind_allowed(kind)?;
        }
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
                    Ok(s) => Ok(ok(Value::Str(s.into()))),
                    Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                }
            }
            ("io", "readline") => {
                use std::io::BufRead;
                let stdin = std::io::stdin();
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) => Ok(Value::Variant { name: "None".into(), args: vec![] }),
                    Ok(_) => {
                        if line.ends_with('\n') { line.pop(); }
                        if line.ends_with('\r') { line.pop(); }
                        Ok(Value::Variant { name: "Some".into(), args: vec![Value::Str(line.into())] })
                    }
                    Err(_) => Ok(Value::Variant { name: "None".into(), args: vec![] }),
                }
            }
            ("io", "argv") => {
                let list: Vec<Value> = self.program_args.iter()
                    .map(|s| Value::Str(s.as_str().into()))
                    .collect();
                Ok(Value::List(list.into()))
            }
            ("io", "write") => {
                let path = expect_str(args.first())?.to_string();
                let contents = expect_str(args.get(1))?.to_string();
                // Honor write-allowlist if any.
                // Canonicalize both sides so macOS /tmp → /private/tmp symlinks
                // and other platform-specific path aliases compare correctly.
                if !self.policy.allow_fs_write.is_empty() {
                    let raw = std::env::current_dir()
                        .map(|cwd| cwd.join(&path))
                        .unwrap_or_else(|_| std::path::PathBuf::from(&path));
                    // canonicalize fails if the file doesn't exist yet (new writes).
                    // Fall back to canonicalizing the parent so macOS /tmp → /private/tmp
                    // symlinks still compare correctly against the allowlist.
                    let p = std::fs::canonicalize(&raw).unwrap_or_else(|_| {
                        raw.parent()
                            .and_then(|par| std::fs::canonicalize(par).ok())
                            .map(|par| par.join(raw.file_name().unwrap_or_default()))
                            .unwrap_or(raw)
                    });
                    let allowed = self.policy.allow_fs_write.iter().any(|a| {
                        let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.clone());
                        p.starts_with(&ca)
                    });
                    if !allowed {
                        return Err(format!("write to `{path}` outside --allow-fs-write"));
                    }
                }
                match std::fs::write(&path, contents) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                }
            }
            ("time", "now") => {
                // LEX_TEST_NOW (Unix seconds) pins for deterministic tests.
                if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                    if let Ok(secs) = s.trim().parse::<i64>() {
                        return Ok(Value::Int(secs));
                    }
                }
                let secs = SystemTime::now().duration_since(UNIX_EPOCH)
                    .map_err(|e| format!("time: {e}"))?.as_secs();
                Ok(Value::Int(secs as i64))
            }
            ("time", "now_ms") => {
                // Unix epoch in milliseconds (#378). `LEX_TEST_NOW` is
                // documented in seconds, so we lift it to ms by *1000
                // to keep the pinning story uniform across `time.now`
                // and `time.now_ms`.
                if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                    if let Ok(secs) = s.trim().parse::<i64>() {
                        return Ok(Value::Int(secs.saturating_mul(1000)));
                    }
                }
                let ms = SystemTime::now().duration_since(UNIX_EPOCH)
                    .map_err(|e| format!("time: {e}"))?.as_millis();
                Ok(Value::Int(ms as i64))
            }
            ("time", "now_str") => {
                // ISO-8601 / RFC 3339 in UTC (#378). Format mirrors
                // `chrono::Utc::now().to_rfc3339()` already used
                // elsewhere in the handler.
                if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                    if let Ok(secs) = s.trim().parse::<i64>() {
                        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
                            .unwrap_or_else(chrono::Utc::now);
                        return Ok(Value::Str(dt.to_rfc3339().into()));
                    }
                }
                Ok(Value::Str(chrono::Utc::now().to_rfc3339().into()))
            }
            ("time", "mono_ns") => {
                // Monotonic clock relative to process start. Cached
                // `Instant::now()` anchor so successive `mono_ns`
                // calls return strictly non-decreasing values without
                // depending on the wall clock. Not affected by
                // `LEX_TEST_NOW` — pinning a monotonic clock would
                // defeat its purpose; tests needing a fake monotonic
                // clock should swap in their own `EffectHandler`.
                static MONO_START: OnceLock<std::time::Instant> = OnceLock::new();
                let start = MONO_START.get_or_init(std::time::Instant::now);
                let dur = std::time::Instant::now().duration_since(*start);
                Ok(Value::Int(dur.as_nanos() as i64))
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
            ("time", "sleep") => {
                // Duration-typed sleep (#445). Duration values are
                // backed by `Int` nanoseconds at runtime (see the
                // `datetime.duration_*` constructors). Same 60s cap
                // as `sleep_ms` — kept consistent so all blocking
                // sleeps share one ceiling.
                let nanos = expect_int(args.first())?;
                if nanos > 0 {
                    let bounded_nanos = (nanos as u64).min(60_000 * 1_000_000);
                    std::thread::sleep(std::time::Duration::from_nanos(bounded_nanos));
                }
                Ok(Value::Unit)
            }
            ("rand", "int_in") => {
                // Honest uniform draw in [lo, hi] inclusive from the OS RNG
                // (#677), replacing the old deterministic midpoint stub.
                // Same entropy source as `crypto.random`; gated `[random]`.
                let lo = expect_int(args.first())?;
                let hi = expect_int(args.get(1))?;
                if hi < lo {
                    return Err(format!("rand.int_in: empty range [{lo}, {hi}]"));
                }
                use rand::{rngs::SysRng, TryRng};
                // span fits in u128 even for the full i64 range; bias from
                // the modulo over a 128-bit draw is < 2^-64 (negligible).
                let span = (hi as i128 - lo as i128 + 1) as u128;
                let mut buf = [0u8; 16];
                SysRng.try_fill_bytes(&mut buf)
                    .map_err(|e| format!("rand.int_in: OS RNG: {e}"))?;
                let draw = (u128::from_le_bytes(buf) % span) as i128;
                Ok(Value::Int((lo as i128 + draw) as i64))
            }
            // `env.get` returns `Option[Str]` — `None` for unset vars.
            // Per-var scoping (`[env(NAME)]`) arrives with #207's
            // per-capability effect parameterization; today the flat
            // `[env]` grants access to the entire process environment.
            ("env", "get") => {
                let name = expect_str(args.first())?;
                Ok(match std::env::var(name) {
                    Ok(v) => Value::Variant {
                        name: "Some".into(),
                        args: vec![Value::Str(v.into())],
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
                serve_http(port, handler_name, program, policy, None, ServeOpts::from_env())
            }
            ("net", "serve_fn") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_fn(port, handler): port must be Int 0..=65535".into()),
                };
                let closure = match args.into_iter().nth(1) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_fn(port, handler): handler must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_fn requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_fn(port, closure, program, policy, ServeOpts::from_env())
            }
            ("net", "serve_routed") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_routed(port, routes, fallback): port must be Int 0..=65535".into()),
                };
                let routes_val = args.get(1).cloned()
                    .ok_or_else(|| "net.serve_routed(port, routes, fallback): missing routes".to_string())?;
                let fallback = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_routed(port, routes, fallback): fallback must be a closure".into()),
                };
                let routes = decode_routes_arg(routes_val)?;
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_routed requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_routed(port, routes, fallback, program, policy, ServeOpts::from_env())
            }
            ("net", "serve_with") => {
                // serve_with(port, handler_name, opts)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_with(port, handler, opts): port must be Int 0..=65535".into()),
                };
                let handler_name = expect_str(args.get(1))?.to_string();
                let opts = decode_serve_opts(args.get(2)
                    .ok_or_else(|| "net.serve_with(port, handler, opts): missing opts".to_string())?)?;
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_with requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http(port, handler_name, program, policy, None, opts)
            }
            ("net", "serve_fn_with") => {
                // serve_fn_with(port, handler_closure, opts)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_fn_with(port, handler, opts): port must be Int 0..=65535".into()),
                };
                let opts = decode_serve_opts(args.get(2)
                    .ok_or_else(|| "net.serve_fn_with(port, handler, opts): missing opts".to_string())?)?;
                let closure = match args.into_iter().nth(1) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_fn_with(port, handler, opts): handler must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_fn_with requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_fn(port, closure, program, policy, opts)
            }
            ("net", "serve_routed_with") => {
                // serve_routed_with(port, routes, fallback, opts)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_routed_with(port, routes, fallback, opts): port must be Int 0..=65535".into()),
                };
                let routes_val = args.get(1).cloned()
                    .ok_or_else(|| "net.serve_routed_with(port, routes, fallback, opts): missing routes".to_string())?;
                let opts = decode_serve_opts(args.get(3)
                    .ok_or_else(|| "net.serve_routed_with(port, routes, fallback, opts): missing opts".to_string())?)?;
                let fallback = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_routed_with(port, routes, fallback, opts): fallback must be a closure".into()),
                };
                let routes = decode_routes_arg(routes_val)?;
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_routed_with requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_routed(port, routes, fallback, program, policy, opts)
            }
            ("net", "serve_quic") => self.dispatch_serve_quic_named(args),
            ("net", "serve_quic_fn") => self.dispatch_serve_quic_fn(args),
            ("net", "serve_quic_routed") => self.dispatch_serve_quic_routed(args),
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
                serve_http(port, handler_name, program, policy, Some(TlsConfig { cert, key }), ServeOpts::from_env())
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
            ("net", "serve_ws_fn") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws_fn(port, subprotocol, handler): port must be Int 0..=65535".into()),
                };
                let subprotocol = expect_str(args.get(1))?.to_string();
                let closure = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn(port, subprotocol, handler): handler must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws_fn requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws_fn(port, subprotocol, closure, program, policy, registry)
            }
            ("net", "serve_ws_fn_auth") => {
                // serve_ws_fn_auth(port, subprotocol, auth, on_message)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws_fn_auth(port, subprotocol, auth, on_message): port must be Int 0..=65535".into()),
                };
                let subprotocol = expect_str(args.get(1))?.to_string();
                let mut it = args.into_iter().skip(2);
                let auth_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_auth(port, subprotocol, auth, on_message): auth must be a closure".into()),
                };
                let handler_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_auth(port, subprotocol, auth, on_message): on_message must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws_fn_auth requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws_fn_auth(
                    port, subprotocol, auth_closure, handler_closure,
                    program, policy, registry,
                )
            }
            ("net", "serve_ws_fn_actor") => {
                // serve_ws_fn_actor(port, subprotocol, name_of, on_message)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws_fn_actor(port, subprotocol, name_of, on_message): port must be Int 0..=65535".into()),
                };
                let subprotocol = expect_str(args.get(1))?.to_string();
                let mut it = args.into_iter().skip(2);
                let name_of_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_actor(port, subprotocol, name_of, on_message): name_of must be a closure".into()),
                };
                let on_message_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_actor(port, subprotocol, name_of, on_message): on_message must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws_fn_actor requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws_fn_actor(
                    port, subprotocol, name_of_closure, on_message_closure,
                    program, policy, registry,
                )
            }
            ("net", "dial_ws") => {
                // dial_ws(url, subprotocol, on_open, on_message)
                let url = expect_str(args.first())?.to_string();
                let subprotocol = expect_str(args.get(1))?.to_string();
                let on_open = match args.get(2).cloned() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws(url, subprotocol, on_open, on_message): on_open must be a closure".into(),
                    ),
                };
                let on_message = match args.into_iter().nth(3) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws(url, subprotocol, on_open, on_message): on_message must be a closure".into(),
                    ),
                };
                let program = self.program.clone().ok_or_else(|| {
                    "net.dial_ws requires a Program reference; use DefaultHandler::with_program".to_string()
                })?;
                let policy = self.policy.clone();
                crate::ws::dial_ws(url, subprotocol, on_open, on_message, program, policy)
            }
            ("net", "dial_ws_actor") => {
                // dial_ws_actor(url, subprotocol, name, on_open, on_message)
                let url = expect_str(args.first())?.to_string();
                let subprotocol = expect_str(args.get(1))?.to_string();
                let name = expect_str(args.get(2))?.to_string();
                let on_open = match args.get(3).cloned() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws_actor(url, subprotocol, name, on_open, on_message): on_open must be a closure".into(),
                    ),
                };
                let on_message = match args.into_iter().nth(4) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws_actor(url, subprotocol, name, on_open, on_message): on_message must be a closure".into(),
                    ),
                };
                let program = self.program.clone().ok_or_else(|| {
                    "net.dial_ws_actor requires a Program reference; use DefaultHandler::with_program".to_string()
                })?;
                let policy = self.policy.clone();
                crate::ws::dial_ws_actor(url, subprotocol, name, on_open, on_message, program, policy)
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
                            "kv.open: `{path}` outside --allow-fs-write").into())));
                    }
                }
                match sled::open(&path) {
                    Ok(db) => {
                        let handle = next_kv_handle();
                        kv_registry().lock().unwrap().insert(handle, db);
                        Ok(ok(Value::Int(handle as i64)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("kv.open: {e}").into()))),
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
                    Err(e) => Ok(err(Value::Str(format!("kv.put: {e}").into()))),
                }
            }
            ("kv", "delete") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.delete: closed or unknown Kv handle".to_string())?;
                match db.remove(key.as_bytes()) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("kv.delete: {e}").into()))),
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
                    keys.push(Value::Str(s.into()));
                }
                Ok(Value::List(keys.into()))
            }
            ("sql", "open") => {
                let path = expect_str(args.first())?.to_string();
                if path.starts_with("postgres://") || path.starts_with("postgresql://") {
                    // Postgres: connect via sync driver; no fs-write policy applies.
                    match postgres::Client::connect(&path, postgres::NoTls) {
                        Ok(client) => {
                            let handle = next_sql_handle();
                            sql_registry().lock().unwrap().insert(handle, SqlConn::Postgres(client));
                            Ok(ok(Value::Int(handle as i64)))
                        }
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.open"))),
                    }
                } else {
                    // SQLite: same shape as `kv.open`; fs-write allowlist applies
                    // (in-memory paths are exempt).
                    if path != ":memory:" && !self.policy.allow_fs_write.is_empty() {
                        let p = std::path::Path::new(&path);
                        if !self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
                            return Ok(err(sql_error(
                                format!("sql.open: `{path}` outside --allow-fs-write"),
                                None, None,
                            )));
                        }
                    }
                    match rusqlite::Connection::open(&path) {
                        Ok(conn) => {
                            let handle = next_sql_handle();
                            sql_registry().lock().unwrap().insert(handle, SqlConn::Sqlite(conn));
                            Ok(ok(Value::Int(handle as i64)))
                        }
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.open"))),
                    }
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
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.exec: closed or unknown Db handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => {
                        let bound = sqlite_params(&params);
                        let bind: Vec<&dyn rusqlite::ToSql> =
                            bound.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
                        match c.execute(&stmt, rusqlite::params_from_iter(bind.iter())) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.exec"))),
                        }
                    }
                    SqlConn::Postgres(c) => {
                        let pg = pg_param_refs(&params);
                        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                            pg.iter().map(|b| b.as_ref()).collect();
                        match c.execute(stmt.as_str(), &refs) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.exec"))),
                        }
                    }
                }
            }
            ("sql", "query") => {
                let h = expect_sql_handle(args.first())?;
                let stmt_str = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.query: closed or unknown Db handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                Ok(match &mut *conn {
                    SqlConn::Sqlite(c)   => sql_run_query_sqlite(c, &stmt_str, &params),
                    SqlConn::Postgres(c) => sql_run_query_pg(c, &stmt_str, &params),
                })
            }
            // Streaming cursor (#379). Allocates an mpsc-backed cursor
            // handle, spawns a producer thread to ship rows one at a
            // time, and returns `__IterCursor(handle)` wrapped in `Ok`.
            // `iter.next` bytecode dispatches the variant tag and
            // effect-calls `sql.cursor_next` (below) to advance.
            ("sql", "query_iter") => {
                let h = expect_sql_handle(args.first())?;
                let stmt_str = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.query_iter: closed or unknown Db handle".to_string())?;

                // Dispatch producer on the connection kind without
                // holding the SqlRegistry lock — the producer thread
                // owns its own clone of the connection Arc.
                let (sender, receiver) = std::sync::mpsc::sync_channel::<Result<Value, String>>(
                    CURSOR_CHANNEL_CAPACITY,
                );
                let cursor_h = next_cursor_handle();
                cursor_registry().lock().unwrap().insert(cursor_h, receiver);

                let arc_for_thread = Arc::clone(&arc);
                // Decide which producer to spawn based on the
                // connection's variant. We can briefly peek at the
                // variant here without holding the lock for the
                // producer's lifetime — the producer locks again
                // inside its thread function.
                let is_sqlite = matches!(*arc.lock().unwrap(), SqlConn::Sqlite(_));
                std::thread::spawn(move || {
                    if is_sqlite {
                        sqlite_cursor_producer(arc_for_thread, stmt_str, params, sender);
                    } else {
                        pg_cursor_producer(arc_for_thread, stmt_str, params, sender);
                    }
                });

                Ok(ok(Value::Variant {
                    name: "__IterCursor".into(),
                    args: vec![Value::Int(cursor_h as i64)],
                }))
            }
            // Pull one row from the producer; called from
            // `iter.next`'s `__IterCursor` dispatch branch. Returns
            // a Lex `Option[Row]`: `Some(row)` while the producer
            // has more, `None` once the channel closes (producer
            // done, errored, or cursor evicted from the registry).
            ("sql", "cursor_next") => {
                let h = match args.first() {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => return Err("sql.cursor_next: expected cursor handle (Int)".into()),
                };
                let rx_arc = match cursor_registry().lock().unwrap().touch_get(h) {
                    Some(a) => a,
                    None => return Ok(Value::Variant { name: "None".into(), args: vec![] }),
                };
                // Lock the receiver itself (separate from the global
                // registry lock) and block on `recv()`. The producer
                // is on a different thread, so this can sleep without
                // contention beyond the per-cursor mutex.
                let recv_result = {
                    let rx = match rx_arc.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    rx.recv()
                };
                match recv_result {
                    Ok(Ok(row)) => Ok(Value::Variant {
                        name: "Some".into(),
                        args: vec![row],
                    }),
                    Ok(Err(_)) | Err(_) => {
                        // Channel closed (producer done) or row error
                        // — drop the registry entry and signal None
                        // so callers stop polling.
                        cursor_registry().lock().unwrap().remove(h);
                        Ok(Value::Variant { name: "None".into(), args: vec![] })
                    }
                }
            }
            // Transactions: begin issues BEGIN SQL on the connection;
            // commit/rollback issue COMMIT/ROLLBACK. SqlTx reuses the
            // same Int handle as Db — the type system enforces correct
            // usage; the runtime treats both as the same registry key.
            ("sql", "begin") => {
                let h = expect_sql_handle(args.first())?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.begin: closed or unknown Db handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => match c.execute_batch("BEGIN") {
                        Ok(()) => Ok(ok(Value::Int(h as i64))),
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.begin"))),
                    },
                    SqlConn::Postgres(c) => match c.batch_execute("BEGIN") {
                        Ok(()) => Ok(ok(Value::Int(h as i64))),
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.begin"))),
                    },
                }
            }
            ("sql", "commit") => {
                let h = expect_sql_handle(args.first())?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.commit: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => match c.execute_batch("COMMIT") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.commit"))),
                    },
                    SqlConn::Postgres(c) => match c.batch_execute("COMMIT") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.commit"))),
                    },
                }
            }
            ("sql", "rollback") => {
                let h = expect_sql_handle(args.first())?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.rollback: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => match c.execute_batch("ROLLBACK") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.rollback"))),
                    },
                    SqlConn::Postgres(c) => match c.batch_execute("ROLLBACK") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.rollback"))),
                    },
                }
            }
            ("sql", "exec_tx") => {
                let h = expect_sql_handle(args.first())?;
                let stmt = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.exec_tx: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => {
                        let bound = sqlite_params(&params);
                        let bind: Vec<&dyn rusqlite::ToSql> =
                            bound.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
                        match c.execute(&stmt, rusqlite::params_from_iter(bind.iter())) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.exec_tx"))),
                        }
                    }
                    SqlConn::Postgres(c) => {
                        let pg = pg_param_refs(&params);
                        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                            pg.iter().map(|b| b.as_ref()).collect();
                        match c.execute(stmt.as_str(), &refs) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.exec_tx"))),
                        }
                    }
                }
            }
            ("sql", "query_tx") => {
                let h = expect_sql_handle(args.first())?;
                let stmt_str = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.query_tx: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                Ok(match &mut *conn {
                    SqlConn::Sqlite(c)   => sql_run_query_sqlite(c, &stmt_str, &params),
                    SqlConn::Postgres(c) => sql_run_query_pg(c, &stmt_str, &params),
                })
            }
            ("sql", "get_str") => Ok(sql_get_col(&args, |v| match v {
                Value::Str(s) => Some(Value::Str(s.clone())),
                Value::Int(n) => Some(Value::Str(n.to_string().into())),
                _ => None,
            })?),
            ("sql", "get_int") => Ok(sql_get_col(&args, |v| match v {
                Value::Int(n) => Some(Value::Int(*n)),
                Value::Float(f) => Some(Value::Int(*f as i64)),
                _ => None,
            })?),
            ("sql", "get_float") => Ok(sql_get_col(&args, |v| match v {
                Value::Float(f) => Some(Value::Float(*f)),
                Value::Int(n)   => Some(Value::Float(*n as f64)),
                _ => None,
            })?),
            ("sql", "get_bool") => Ok(sql_get_col(&args, |v| match v {
                Value::Bool(b)  => Some(Value::Bool(*b)),
                Value::Int(n)   => Some(Value::Bool(*n != 0)),
                _ => None,
            })?),

            // ── std.redis (#533) ─────────────────────────────────────────
            //
            // ConnRedis is an opaque Int handle into the global RedisRegistry.
            // All ops carry [net] — Redis is a TCP service.
            //
            // subscribe/psubscribe open a *dedicated* connection so they don't
            // interfere with the handle's regular connection. Redis disallows
            // non-Pub/Sub commands on a subscribed connection.
            ("redis", "connect") => {
                let url = expect_str(args.first())?.to_string();
                self.ensure_host_allowed(&url)?;
                match redis::Client::open(url.as_str()) {
                    Ok(client) => match client.get_connection() {
                        Ok(conn) => {
                            let handle = next_redis_handle();
                            redis_registry().lock().unwrap().insert(handle, RedisEntry { url, conn });
                            Ok(ok(Value::Int(handle as i64)))
                        }
                        Err(e) => Ok(err(Value::Str(format!("redis.connect: {e}").into()))),
                    },
                    Err(e) => Ok(err(Value::Str(format!("redis.connect: {e}").into()))),
                }
            }
            ("redis", "close") => {
                let h = expect_redis_handle(args.first())?;
                redis_registry().lock().unwrap().remove(h);
                Ok(Value::Unit)
            }
            ("redis", "get") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.get: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                match entry.conn.get::<_, Option<String>>(&key) {
                    Ok(Some(v)) => Ok(some(Value::Str(v.into()))),
                    Ok(None)    => Ok(none()),
                    Err(e)      => Err(format!("redis.get: {e}")),
                }
            }
            ("redis", "set") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.set: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.set::<_, _, ()>(&key, &val)
                    .map_err(|e| format!("redis.set: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "set_ex") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let ttl = expect_int(args.get(3))?;
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.set_ex: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.set_ex::<_, _, ()>(&key, &val, ttl as u64)
                    .map_err(|e| format!("redis.set_ex: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "del") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.del: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.del::<_, ()>(&key)
                    .map_err(|e| format!("redis.del: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "exists") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.exists: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let present: bool = entry.conn.exists(&key)
                    .map_err(|e| format!("redis.exists: {e}"))?;
                Ok(Value::Bool(present))
            }
            ("redis", "expire") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let ttl = expect_int(args.get(2))?;
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.expire: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.expire::<_, ()>(&key, ttl)
                    .map_err(|e| format!("redis.expire: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "publish") => {
                let h = expect_redis_handle(args.first())?;
                let channel = expect_str(args.get(1))?.to_string();
                let msg = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.publish: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.publish(&channel, &msg)
                    .map_err(|e| format!("redis.publish: {e}"))?;
                Ok(Value::Int(n))
            }
            // subscribe / psubscribe: blocking loops on dedicated connections.
            // Each inbound message calls the Lex closure in a fresh VM built
            // from `self.program` — same pattern as net.serve_fn's per-request
            // dispatch. Returns Unit (Nil) only if the connection drops.
            ("redis", "subscribe") => {
                let h = expect_redis_handle(args.first())?;
                let channel = expect_str(args.get(1))?.to_string();
                let closure = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("redis.subscribe: handler must be a Closure".into()),
                };
                let program = self.program.clone()
                    .ok_or("redis.subscribe: no program; call DefaultHandler::with_program")?;
                let policy = self.policy.clone();
                let url = redis_registry().lock().unwrap()
                    .get_url(h)
                    .ok_or("redis.subscribe: closed or unknown ConnRedis handle")?;
                let client = redis::Client::open(url.as_str())
                    .map_err(|e| format!("redis.subscribe: {e}"))?;
                let mut conn = client.get_connection()
                    .map_err(|e| format!("redis.subscribe: {e}"))?;
                let mut pubsub = conn.as_pubsub();
                pubsub.subscribe(&channel)
                    .map_err(|e| format!("redis.subscribe: {e}"))?;
                loop {
                    let msg = pubsub.get_message()
                        .map_err(|e| format!("redis.subscribe: {e}"))?;
                    let ch: String = msg.get_channel_name().to_string();
                    let payload: String = msg.get_payload()
                        .map_err(|e| format!("redis.subscribe: payload: {e}"))?;
                    let handler = DefaultHandler::new(policy.clone())
                        .with_program(Arc::clone(&program));
                    let mut vm = Vm::with_handler(&program, Box::new(handler));
                    vm.invoke_closure_value(closure.clone(), vec![
                        Value::Str(ch.into()),
                        Value::Str(payload.into()),
                    ]).map_err(|e| format!("redis.subscribe: handler: {e:?}"))?;
                }
            }
            ("redis", "psubscribe") => {
                let h = expect_redis_handle(args.first())?;
                let pattern = expect_str(args.get(1))?.to_string();
                let closure = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("redis.psubscribe: handler must be a Closure".into()),
                };
                let program = self.program.clone()
                    .ok_or("redis.psubscribe: no program; call DefaultHandler::with_program")?;
                let policy = self.policy.clone();
                let url = redis_registry().lock().unwrap()
                    .get_url(h)
                    .ok_or("redis.psubscribe: closed or unknown ConnRedis handle")?;
                let client = redis::Client::open(url.as_str())
                    .map_err(|e| format!("redis.psubscribe: {e}"))?;
                let mut conn = client.get_connection()
                    .map_err(|e| format!("redis.psubscribe: {e}"))?;
                let mut pubsub = conn.as_pubsub();
                pubsub.psubscribe(&pattern)
                    .map_err(|e| format!("redis.psubscribe: {e}"))?;
                loop {
                    let msg = pubsub.get_message()
                        .map_err(|e| format!("redis.psubscribe: {e}"))?;
                    let pat: String = msg.get_pattern()
                        .ok()
                        .and_then(|v: Option<String>| v)
                        .unwrap_or_else(|| pattern.clone());
                    let ch: String = msg.get_channel_name().to_string();
                    let payload: String = msg.get_payload()
                        .map_err(|e| format!("redis.psubscribe: payload: {e}"))?;
                    let handler = DefaultHandler::new(policy.clone())
                        .with_program(Arc::clone(&program));
                    let mut vm = Vm::with_handler(&program, Box::new(handler));
                    vm.invoke_closure_value(closure.clone(), vec![
                        Value::Str(pat.into()),
                        Value::Str(ch.into()),
                        Value::Str(payload.into()),
                    ]).map_err(|e| format!("redis.psubscribe: handler: {e:?}"))?;
                }
            }
            ("redis", "lpush") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.lpush: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.lpush(&key, &val)
                    .map_err(|e| format!("redis.lpush: {e}"))?;
                Ok(Value::Int(n))
            }
            ("redis", "rpush") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.rpush: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.rpush(&key, &val)
                    .map_err(|e| format!("redis.rpush: {e}"))?;
                Ok(Value::Int(n))
            }
            ("redis", "brpop") => {
                // timeout=0 means block indefinitely; the Lex runtime does not
                // treat this as a hung effect — it is the caller's intent.
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let timeout = expect_int(args.get(2))?;
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.brpop: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                // brpop returns Option<(String, String)>: (key, value).
                // We surface only the value to the Lex caller.
                let result: Option<(String, String)> = entry.conn
                    .brpop(&key, timeout as f64)
                    .map_err(|e| format!("redis.brpop: {e}"))?;
                match result {
                    Some((_, v)) => Ok(some(Value::Str(v.into()))),
                    None         => Ok(none()),
                }
            }
            ("redis", "llen") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.llen: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.llen(&key)
                    .map_err(|e| format!("redis.llen: {e}"))?;
                Ok(Value::Int(n))
            }
            ("redis", "hset") => {
                let h = expect_redis_handle(args.first())?;
                let key   = expect_str(args.get(1))?.to_string();
                let field = expect_str(args.get(2))?.to_string();
                let val   = expect_str(args.get(3))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hset: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.hset::<_, _, _, ()>(&key, &field, &val)
                    .map_err(|e| format!("redis.hset: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "hget") => {
                let h = expect_redis_handle(args.first())?;
                let key   = expect_str(args.get(1))?.to_string();
                let field = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hget: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                match entry.conn.hget::<_, _, Option<String>>(&key, &field) {
                    Ok(Some(v)) => Ok(some(Value::Str(v.into()))),
                    Ok(None)    => Ok(none()),
                    Err(e)      => Err(format!("redis.hget: {e}")),
                }
            }
            ("redis", "hdel") => {
                let h = expect_redis_handle(args.first())?;
                let key   = expect_str(args.get(1))?.to_string();
                let field = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hdel: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.hdel::<_, _, ()>(&key, &field)
                    .map_err(|e| format!("redis.hdel: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "hgetall") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hgetall: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let map: std::collections::HashMap<String, String> = entry.conn
                    .hgetall(&key)
                    .map_err(|e| format!("redis.hgetall: {e}"))?;
                let pairs: Vec<Value> = map.into_iter()
                    .map(|(k, v)| Value::Tuple(vec![Value::Str(k.into()), Value::Str(v.into())]))
                    .collect();
                Ok(Value::List(pairs.into()))
            }

            // `proc.spawn` was removed with the `std.proc` module (#678);
            // the blocking-capture path now lives at `process.run`, handled
            // in the `kind == "process"` block above.
            other => Err(format!("unsupported effect {}.{}", other.0, other.1)),
        }
    }

    /// `list.par_map` worker-handler factory (#305 slice 2).
    ///
    /// Builds a fresh `DefaultHandler` per worker that shares the
    /// budget pool with the parent (`Arc<AtomicU64>`) so a parallel
    /// batch can't escape the run-wide budget ceiling. Other state
    /// is intentionally split per-worker:
    ///
    /// - `sink`: a `StdoutSink` per worker. Tests that capture
    ///   output via a `SharedSink` wrapped in `Arc<Mutex<…>>` see
    ///   each worker as a fresh handler. Print interleaving on
    ///   stdout is acceptable; tests that need ordered capture run
    ///   workloads serially anyway.
    /// - `mcp_clients`: a fresh per-worker LRU cache. The parent's
    ///   subprocess handles can't be shared across threads without
    ///   mutex-serialising every MCP call, which would defeat the
    ///   parallelism. Cache hit rate is sub-optimal across the
    ///   first call per worker; warmed caches still amortise within
    ///   a worker.
    /// - `chat_registry`: cloned `Arc<ChatRegistry>` so all workers
    ///   route into the same chat dispatch layer.
    /// - `program`: cloned `Arc<Program>` so `net.serve` (if a
    ///   worker invokes it) sees the same compiled program.
    fn spawn_for_worker(&self) -> Option<Box<dyn lex_bytecode::vm::EffectHandler + Send>> {
        let mut fresh = DefaultHandler::new(self.policy.clone());
        // Share the budget pool atomically — slice 2's correctness
        // contract: parallel work counts against the same ceiling.
        fresh.budget_remaining = std::sync::Arc::clone(&self.budget_remaining);
        fresh.budget_ceiling = self.budget_ceiling;
        fresh.read_root = self.read_root.clone();
        fresh.program = self.program.clone();
        fresh.chat_registry = self.chat_registry.clone();
        // #305 slice 3: share the stream registry across workers so
        // a stream produced on one thread (or the parent) is
        // consumable on any other. The registry is already
        // `Arc<Mutex<…>>` so concurrent access is safe.
        fresh.streams = std::sync::Arc::clone(&self.streams);
        fresh.next_stream_id = std::sync::Arc::clone(&self.next_stream_id);
        fresh.program_args = self.program_args.clone();
        Some(Box::new(fresh))
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
    opts: ServeOpts,
) -> Result<Value, String> {
    match tls {
        None => serve_http_plain(port, handler_name, program, policy, opts),
        Some(cfg) => serve_http_tls_legacy(port, handler_name, program, policy, cfg),
    }
}

/// Hyper 1.x + Tokio multi-thread HTTP/1.1 server for `net.serve`.
/// Each connection is accepted in an async task; the synchronous Lex VM
/// call runs inside `spawn_blocking` so it doesn't block the executor.
///
/// `LEX_NET_INLINE_VM=1` (or `=true`) skips the `spawn_blocking` hop and
/// runs the VM directly on the tokio worker. Faster for handlers that
/// return in tens of microseconds; pathological if handlers do real
/// CPU/blocking work, since they stall the worker. Experimental — see
/// lex-lang issue #431.
fn serve_http_plain(
    port: u16,
    handler_name: String,
    program: Arc<Program>,
    policy: Policy,
    opts: ServeOpts,
) -> Result<Value, String> {
    use http_body_util::BodyExt as _;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto;
    use tokio::net::TcpListener as TokioTcpListener;

    let inline_vm = opts.inline_vm;
    let http2 = opts.http2;
    let host = opts.host.clone();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("net.serve: tokio runtime: {e}"))?;
    rt.block_on(async move {
        let listener = TokioTcpListener::bind((host.as_str(), port))
            .await
            .map_err(|e| format!("net.serve bind {host}:{port}: {e}"))?;
        eprintln!(
            "net.serve: listening on http://{host}:{port}{}{}",
            if inline_vm { " (inline-vm)" } else { "" },
            if http2 { " (http1+http2)" } else { "" }
        );
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("net.serve accept: {e}"))?;
            let io = TokioIo::new(stream);
            let program = Arc::clone(&program);
            let policy = policy.clone();
            let handler_name = handler_name.clone();
            tokio::spawn(async move {
                let program2 = Arc::clone(&program);
                let policy2 = policy.clone();
                let handler_name2 = handler_name.clone();
                let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let program = Arc::clone(&program2);
                    let policy = policy2.clone();
                    let handler_name = handler_name2.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let body_bytes = body
                            .collect()
                            .await
                            .map(|c| c.to_bytes())
                            .unwrap_or_default();
                        let result = if inline_vm {
                            // Inline path — run the VM on this tokio worker.
                            // Cheap when handlers return in microseconds; will
                            // stall the worker on heavy handlers (caveat per #431).
                            let lex_req = build_request_value_parts(&parts, &body_bytes);
                            let handler = DefaultHandler::new(policy)
                                .with_program(Arc::clone(&program));
                            let mut vm = Vm::with_handler(&program, Box::new(handler));
                            let r = vm.call(&handler_name, vec![lex_req]);
                            // Unpack inline so the VM is still in
                            // scope (#463 wire-up).
                            Ok(r.map(|v| unpack_response(&mut vm, &v)))
                        } else {
                            tokio::task::spawn_blocking(move || {
                                let lex_req = build_request_value_parts(&parts, &body_bytes);
                                let handler = DefaultHandler::new(policy)
                                    .with_program(Arc::clone(&program));
                                let mut vm = Vm::with_handler(&program, Box::new(handler));
                                let r = vm.call(&handler_name, vec![lex_req]);
                                r.map(|v| unpack_response(&mut vm, &v))
                            })
                            .await
                        };
                        Ok::<_, std::convert::Infallible>(match result {
                            Ok(Ok(unpacked)) => build_hyper_response(unpacked),
                            Ok(Err(e)) => error_response(500, &format!("internal error: {e}")),
                            Err(e) => error_response(500, &format!("task panicked: {e}")),
                        })
                    }
                });
                let result = if http2 {
                    auto::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    http1::Builder::new()
                        .serve_connection(io, svc)
                        .await
                        .map_err(|e| e.to_string())
                };
                if let Err(e) = result {
                    eprintln!("net.serve: connection error: {e}");
                }
            });
        }
    })
}

/// TLS path: still uses tiny_http pending a tokio-rustls migration.
fn serve_http_tls_legacy(
    port: u16,
    handler_name: String,
    program: Arc<Program>,
    policy: Policy,
    cfg: TlsConfig,
) -> Result<Value, String> {
    let ssl = tiny_http::SslConfig {
        certificate: cfg.cert,
        private_key: cfg.key,
    };
    let server = tiny_http::Server::https(("0.0.0.0", port), ssl)
        .map_err(|e| format!("net.serve_tls bind {port}: {e}"))?;
    eprintln!("net.serve: listening on https://0.0.0.0:{port}");
    for req in server.incoming_requests() {
        let program = Arc::clone(&program);
        let policy = policy.clone();
        let handler_name = handler_name.clone();
        std::thread::spawn(move || handle_request_tls(req, program, policy, handler_name));
    }
    Ok(Value::Unit)
}

fn handle_request_tls(
    mut req: tiny_http::Request,
    program: Arc<Program>,
    policy: Policy,
    handler_name: String,
) {
    let lex_req = build_request_value_tiny(&mut req);
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&program));
    let mut vm = Vm::with_handler(&program, Box::new(handler));
    match vm.call(&handler_name, vec![lex_req]) {
        Ok(resp) => {
            // Drain lazy iters + read response fields straight out of
            // any arena handles while the VM is still in scope — see
            // #477 and `docs/design/arena-plumbing.md` § "Status
            // update (2026-06-05)" for why this is a single fused
            // step now.
            let (status, body, headers) = unpack_response(&mut vm, &resp);
            respond_with_body_tls(req, status, body, headers);
        }
        Err(e) => {
            let response = tiny_http::Response::from_string(format!("internal error: {e}"))
                .with_status_code(500);
            let _ = req.respond(response);
        }
    }
}

/// Hyper 1.x + Tokio multi-thread HTTP/1.1 server for `net.serve_fn`.
///
/// `LEX_NET_INLINE_VM=1` skips `spawn_blocking` — see `serve_http_plain`'s
/// doc-comment for the tradeoffs. Same env var gates both paths.
fn serve_http_fn(
    port: u16,
    closure: Value,
    program: Arc<Program>,
    policy: Policy,
    opts: ServeOpts,
) -> Result<Value, String> {
    use http_body_util::BodyExt as _;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto;
    use tokio::net::TcpListener as TokioTcpListener;

    let inline_vm = opts.inline_vm;
    let http2 = opts.http2;
    let host = opts.host.clone();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("net.serve_fn: tokio runtime: {e}"))?;
    rt.block_on(async move {
        let listener = TokioTcpListener::bind((host.as_str(), port))
            .await
            .map_err(|e| format!("net.serve_fn bind {host}:{port}: {e}"))?;
        eprintln!(
            "net.serve_fn: listening on http://{host}:{port}{}{}",
            if inline_vm { " (inline-vm)" } else { "" },
            if http2 { " (http1+http2)" } else { "" }
        );
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("net.serve_fn accept: {e}"))?;
            let io = TokioIo::new(stream);
            let program = Arc::clone(&program);
            let policy = policy.clone();
            let closure = closure.clone();
            tokio::spawn(async move {
                let program2 = Arc::clone(&program);
                let policy2 = policy.clone();
                let closure2 = closure.clone();
                let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let program = Arc::clone(&program2);
                    let policy = policy2.clone();
                    let closure = closure2.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let body_bytes = body
                            .collect()
                            .await
                            .map(|c| c.to_bytes())
                            .unwrap_or_default();
                        let result = if inline_vm {
                            let lex_req = build_request_value_parts(&parts, &body_bytes);
                            let handler = DefaultHandler::new(policy)
                                .with_program(Arc::clone(&program));
                            let mut vm = Vm::with_handler(&program, Box::new(handler));
                            // #463 scaffolding — bracket the user
                            // handler with a request scope so the
                            // arena lifecycle is exercised. The
                            // arena itself is unused today; this
                            // proves the lifecycle is sound for the
                            // follow-on Value-rep slice.
                            let scope = vm.enter_request_scope();
                            let r = vm.invoke_closure_value(closure, vec![lex_req]);
                            // Unpack inline so the VM is still in
                            // scope for both lazy-iter draining and
                            // slab-direct field reads (#463).
                            let r = r.map(|v| unpack_response(&mut vm, &v));
                            vm.exit_request_scope(scope);
                            Ok(r)
                        } else {
                            tokio::task::spawn_blocking(move || {
                                let lex_req = build_request_value_parts(&parts, &body_bytes);
                                let handler = DefaultHandler::new(policy)
                                    .with_program(Arc::clone(&program));
                                let mut vm = Vm::with_handler(&program, Box::new(handler));
                                let scope = vm.enter_request_scope();
                                let r = vm.invoke_closure_value(closure, vec![lex_req]);
                                let r = r.map(|v| unpack_response(&mut vm, &v));
                                vm.exit_request_scope(scope);
                                r
                            })
                            .await
                        };
                        Ok::<_, std::convert::Infallible>(match result {
                            Ok(Ok(unpacked)) => build_hyper_response(unpacked),
                            Ok(Err(e)) => error_response(500, &format!("internal error: {e}")),
                            Err(e) => error_response(500, &format!("task panicked: {e}")),
                        })
                    }
                });
                let result = if http2 {
                    auto::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    http1::Builder::new()
                        .serve_connection(io, svc)
                        .await
                        .map_err(|e| e.to_string())
                };
                if let Err(e) = result {
                    eprintln!("net.serve_fn: connection error: {e}");
                }
            });
        }
    })
}

/// Compiled segment of a route pattern. Patterns are split on `/`
/// once at registration time so the per-request match loop is just a
/// length check + segment-by-segment compare.
#[derive(Clone, Debug)]
pub(crate) enum RouteSeg {
    Literal(String),
    /// `:name` capture — binds the request segment under `name` in
    /// `req.path_params`.
    Param(String),
}

/// Compile a `:name`-style pattern (e.g. `"/users/:id/posts"`) into a
/// segment list. Errors out at registration time so bad patterns
/// surface before the server binds, not on the first matching request.
fn compile_path_pattern(pat: &str) -> Result<Vec<RouteSeg>, String> {
    if pat.is_empty() {
        return Err("path pattern must be non-empty (use \"/\" for the root)".into());
    }
    if !pat.starts_with('/') {
        return Err(format!("path pattern must start with '/' (got {pat:?})"));
    }
    let mut segs = Vec::new();
    for raw in pat.split('/') {
        if let Some(name) = raw.strip_prefix(':') {
            if name.is_empty() {
                return Err(format!(
                    ":-segment in pattern {pat:?} must have a name (e.g. :id)"
                ));
            }
            segs.push(RouteSeg::Param(name.to_string()));
        } else {
            segs.push(RouteSeg::Literal(raw.to_string()));
        }
    }
    Ok(segs)
}

/// Attempt to match a request `path` against a compiled pattern. On
/// success returns the captured `:name` segments as a Lex-shaped map
/// keyed by `MapKey::Str(name)`; on mismatch returns `None`. Strict
/// segment-count match: trailing slashes matter (caller registers
/// both forms if both should match).
fn match_path_pattern(
    segs: &[RouteSeg],
    path: &str,
) -> Option<std::collections::BTreeMap<lex_bytecode::MapKey, Value>> {
    let path_segs: Vec<&str> = path.split('/').collect();
    if path_segs.len() != segs.len() {
        return None;
    }
    let mut params = std::collections::BTreeMap::new();
    for (pat, p) in segs.iter().zip(path_segs.iter()) {
        match pat {
            RouteSeg::Literal(lit) => {
                if lit != p {
                    return None;
                }
            }
            RouteSeg::Param(name) => {
                params.insert(
                    lex_bytecode::MapKey::Str(name.clone()),
                    Value::Str((*p).into()),
                );
            }
        }
    }
    Some(params)
}

/// Decode the `routes` argument of `net.serve_routed` into a vector
/// of `(uppercased-method-or-"*", compiled-pattern, handler-closure)`.
/// Validates and pre-compiles up front so malformed routes fail before
/// the server starts.
fn decode_routes_arg(
    v: Value,
) -> Result<Vec<(String, Vec<RouteSeg>, Value)>, String> {
    let list = match v {
        Value::List(xs) => xs,
        _ => return Err("net.serve_routed: routes must be a List".into()),
    };
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.into_iter().enumerate() {
        let tup = match item {
            Value::Tuple(xs) if xs.len() == 3 => xs,
            other => return Err(format!(
                "net.serve_routed: route #{i} must be a (method, pattern, handler) 3-tuple, got {other:?}"
            )),
        };
        let mut it = tup.into_iter();
        let method_raw = match it.next() {
            Some(Value::Str(s)) => s.to_string(),
            _ => return Err(format!("net.serve_routed: route #{i} method must be Str")),
        };
        // Normalise method to uppercase for matching. "*" stays as-is.
        let method = if method_raw == "*" { method_raw } else { method_raw.to_uppercase() };
        let pattern = match it.next() {
            Some(Value::Str(s)) => s.to_string(),
            _ => return Err(format!("net.serve_routed: route #{i} path-pattern must be Str")),
        };
        let segs = compile_path_pattern(&pattern)
            .map_err(|e| format!("net.serve_routed: route #{i} ({pattern:?}): {e}"))?;
        let closure = match it.next() {
            Some(c @ Value::Closure { .. }) => c,
            _ => return Err(format!("net.serve_routed: route #{i} handler must be a closure")),
        };
        out.push((method, segs, closure));
    }
    Ok(out)
}

/// Pick the first matching route for `(method, path)` and return its
/// handler closure plus captured path-params. Method match is
/// case-insensitive vs the request (already uppercased at decode
/// time); `"*"` in a route matches any method.
pub(crate) fn dispatch_route<'a>(
    routes: &'a [(String, Vec<RouteSeg>, Value)],
    req_method: &str,
    req_path: &str,
) -> Option<(&'a Value, std::collections::BTreeMap<lex_bytecode::MapKey, Value>)> {
    let req_method_upper = req_method.to_ascii_uppercase();
    for (m, segs, closure) in routes {
        if m != "*" && m != &req_method_upper {
            continue;
        }
        if let Some(params) = match_path_pattern(segs, req_path) {
            return Some((closure, params));
        }
    }
    None
}

/// Overwrite the `path_params` field on a Request record with the
/// captured map. Request records are always built with an empty
/// `path_params` field, so this just updates the existing slot.
pub(crate) fn stamp_path_params(
    req: &mut Value,
    params: std::collections::BTreeMap<lex_bytecode::MapKey, Value>,
) {
    if let Value::Record { fields: rec, .. } = req {
        rec.insert("path_params".into(), Value::Map(params));
    }
}

/// Hyper 1.x + Tokio multi-thread HTTP/1.1 server for `net.serve_routed`.
/// Mirrors `serve_http_fn` (#431 inline-vm gate also applies); the only
/// difference is that route dispatch picks the closure per-request from
/// the precompiled `routes` table, falling back to the `fallback`
/// closure when no route matches.
fn serve_http_routed(
    port: u16,
    routes: Vec<(String, Vec<RouteSeg>, Value)>,
    fallback: Value,
    program: Arc<Program>,
    policy: Policy,
    opts: ServeOpts,
) -> Result<Value, String> {
    use http_body_util::BodyExt as _;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto;
    use tokio::net::TcpListener as TokioTcpListener;

    let inline_vm = opts.inline_vm;
    let http2 = opts.http2;
    let host = opts.host.clone();
    let routes = Arc::new(routes);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("net.serve_routed: tokio runtime: {e}"))?;
    rt.block_on(async move {
        let listener = TokioTcpListener::bind((host.as_str(), port))
            .await
            .map_err(|e| format!("net.serve_routed bind {host}:{port}: {e}"))?;
        eprintln!(
            "net.serve_routed: listening on http://{host}:{port} ({} routes{}{})",
            routes.len(),
            if inline_vm { ", inline-vm" } else { "" },
            if http2 { ", http1+http2" } else { "" }
        );
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("net.serve_routed accept: {e}"))?;
            let io = TokioIo::new(stream);
            let program = Arc::clone(&program);
            let policy = policy.clone();
            let routes = Arc::clone(&routes);
            let fallback = fallback.clone();
            tokio::spawn(async move {
                let program2 = Arc::clone(&program);
                let policy2 = policy.clone();
                let routes2 = Arc::clone(&routes);
                let fallback2 = fallback.clone();
                let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let program = Arc::clone(&program2);
                    let policy = policy2.clone();
                    let routes = Arc::clone(&routes2);
                    let fallback = fallback2.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let body_bytes = body
                            .collect()
                            .await
                            .map(|c| c.to_bytes())
                            .unwrap_or_default();
                        let method = parts.method.as_str().to_string();
                        let path = match parts.uri.path() {
                            "" => "/".to_string(),
                            p => p.to_string(),
                        };
                        let result = if inline_vm {
                            let mut lex_req = build_request_value_parts(&parts, &body_bytes);
                            let (closure, params) = match dispatch_route(&routes, &method, &path) {
                                Some((c, p)) => (c.clone(), p),
                                None => (fallback.clone(), std::collections::BTreeMap::new()),
                            };
                            stamp_path_params(&mut lex_req, params);
                            let handler = DefaultHandler::new(policy)
                                .with_program(Arc::clone(&program));
                            let mut vm = Vm::with_handler(&program, Box::new(handler));
                            let r = vm.invoke_closure_value(closure, vec![lex_req]);
                            // Unpack inline so the VM is still in
                            // scope (#463 wire-up, see arena-plumbing.md).
                            Ok(r.map(|v| unpack_response(&mut vm, &v)))
                        } else {
                            tokio::task::spawn_blocking(move || {
                                let mut lex_req = build_request_value_parts(&parts, &body_bytes);
                                let (closure, params) = match dispatch_route(&routes, &method, &path) {
                                    Some((c, p)) => (c.clone(), p),
                                    None => (fallback.clone(), std::collections::BTreeMap::new()),
                                };
                                stamp_path_params(&mut lex_req, params);
                                let handler = DefaultHandler::new(policy)
                                    .with_program(Arc::clone(&program));
                                let mut vm = Vm::with_handler(&program, Box::new(handler));
                                let r = vm.invoke_closure_value(closure, vec![lex_req]);
                                r.map(|v| unpack_response(&mut vm, &v))
                            })
                            .await
                        };
                        Ok::<_, std::convert::Infallible>(match result {
                            Ok(Ok(unpacked)) => build_hyper_response(unpacked),
                            Ok(Err(e)) => error_response(500, &format!("internal error: {e}")),
                            Err(e) => error_response(500, &format!("task panicked: {e}")),
                        })
                    }
                });
                let result = if http2 {
                    auto::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    http1::Builder::new()
                        .serve_connection(io, svc)
                        .await
                        .map_err(|e| e.to_string())
                };
                if let Err(e) = result {
                    eprintln!("net.serve_routed: connection error: {e}");
                }
            });
        }
    })
}

/// Read `LEX_NET_INLINE_VM` and report whether the runtime should skip
/// `spawn_blocking` on the per-request VM call. Accepts `1` / `true`
/// (case-insensitive); anything else (including unset) keeps the
/// default `spawn_blocking` behaviour. See issue #431.
fn env_inline_vm() -> bool {
    match std::env::var("LEX_NET_INLINE_VM") {
        Ok(v) => {
            let s = v.trim().to_ascii_lowercase();
            s == "1" || s == "true"
        }
        Err(_) => false,
    }
}

/// Server-config record threaded through `serve_http_plain` / `_fn` /
/// `_routed`. Built from env vars on the legacy `net.serve*` paths
/// (`ServeOpts::from_env`) or decoded from a user-supplied Lex record
/// literal on the new `net.serve*_with` paths (`decode_serve_opts`).
/// See lex-lang#497 for the design rationale.
#[derive(Debug, Clone)]
pub(crate) struct ServeOpts {
    pub(crate) http2: bool,
    pub(crate) inline_vm: bool,
    pub(crate) host: String,
}

impl ServeOpts {
    /// Default values that match the legacy behaviour with env vars
    /// honoured. Use this when entering via `net.serve`, `net.serve_fn`,
    /// or `net.serve_routed` — preserves backwards compatibility.
    fn from_env() -> Self {
        Self {
            http2: env_http2(),
            inline_vm: env_inline_vm(),
            host: "0.0.0.0".to_string(),
        }
    }

    /// Hard-coded defaults returned by `net.default_opts()`. Does NOT
    /// consult env vars — the `*_with` paths read the opts record
    /// literally, so the env-var escape hatch only applies to legacy
    /// callers (`net.serve` et al).
    fn lex_defaults() -> Self {
        Self {
            http2: false,
            inline_vm: false,
            host: "0.0.0.0".to_string(),
        }
    }

    /// Convert to a Lex `Value::Record` for return from `default_opts()`.
    fn to_value(&self) -> Value {
        let mut rec = indexmap::IndexMap::new();
        rec.insert("http2".to_string(),     Value::Bool(self.http2));
        rec.insert("inline_vm".to_string(), Value::Bool(self.inline_vm));
        rec.insert("host".to_string(),      Value::Str(self.host.clone().into()));
        Value::record_dynamic(rec)
    }
}

/// Decode a `ServeOpts` from a Lex record literal. Fields are
/// required — the type-checker has already verified the shape, so
/// here we just project them out. Any deviation from the expected
/// shape is treated as an internal-consistency error.
fn decode_serve_opts(v: &Value) -> Result<ServeOpts, String> {
    let rec = match v {
        Value::Record { fields: r, .. } => r,
        other => return Err(format!("opts must be a Record, got {other:?}")),
    };
    let http2 = match rec.get("http2") {
        Some(Value::Bool(b)) => *b,
        _ => return Err("opts.http2 must be Bool".into()),
    };
    let inline_vm = match rec.get("inline_vm") {
        Some(Value::Bool(b)) => *b,
        _ => return Err("opts.inline_vm must be Bool".into()),
    };
    let host = match rec.get("host") {
        Some(Value::Str(s)) => s.to_string(),
        _ => return Err("opts.host must be Str".into()),
    };
    Ok(ServeOpts { http2, inline_vm, host })
}

// ── tls.* and net.serve_quic* dispatch helpers (#496) ──────────────
//
// `TlsConfig` is opaque in the type system (a `Ty::Con("TlsConfig",…)`)
// but at runtime it's a `Value::Record({cert :: Bytes, key :: Bytes})`
// carrying the PEM-encoded chain + private key. The opacity matters
// because we may switch the in-runtime representation to a Resource
// handle later (e.g. to keep the private key out of GC-visible
// memory) without breaking source code.

fn make_tls_config_value(cert_pem: Vec<u8>, key_pem: Vec<u8>) -> Value {
    let mut rec = indexmap::IndexMap::new();
    rec.insert("cert".into(), Value::Bytes(cert_pem));
    rec.insert("key".into(),  Value::Bytes(key_pem));
    Value::record_dynamic(rec)
}

#[cfg(feature = "quic")]
fn decode_tls_config(v: &Value) -> Result<crate::quic::QuicTls, String> {
    let rec = match v {
        Value::Record { fields: r, .. } => r,
        other => return Err(format!("TlsConfig: expected Record, got {other:?}")),
    };
    let cert = match rec.get("cert") {
        Some(Value::Bytes(b)) => b.to_vec(),
        _ => return Err("TlsConfig.cert: must be Bytes".into()),
    };
    let key = match rec.get("key") {
        Some(Value::Bytes(b)) => b.to_vec(),
        _ => return Err("TlsConfig.key: must be Bytes".into()),
    };
    Ok(crate::quic::QuicTls { cert_pem: cert, key_pem: key })
}

fn dispatch_tls_from_pem_files(
    handler: &DefaultHandler,
    args: Vec<Value>,
) -> Result<Value, String> {
    let cert_path = expect_str(args.first())?.to_string();
    let key_path  = expect_str(args.get(1))?.to_string();
    let cert_resolved = handler.resolve_read_path(&cert_path);
    let key_resolved  = handler.resolve_read_path(&key_path);
    if !handler.policy.allow_fs_read.is_empty() {
        let allowed = |p: &std::path::Path| -> bool {
            handler.policy.allow_fs_read.iter().any(|a| p.starts_with(a))
        };
        if !allowed(&cert_resolved) {
            return Ok(err(Value::Str(
                format!("tls.from_pem_files: cert `{cert_path}` outside --allow-fs-read").into(),
            )));
        }
        if !allowed(&key_resolved) {
            return Ok(err(Value::Str(
                format!("tls.from_pem_files: key `{key_path}` outside --allow-fs-read").into(),
            )));
        }
    }
    let cert = match std::fs::read(&cert_resolved) {
        Ok(b) => b,
        Err(e) => return Ok(err(Value::Str(format!("read cert {cert_path}: {e}").into()))),
    };
    let key = match std::fs::read(&key_resolved) {
        Ok(b) => b,
        Err(e) => return Ok(err(Value::Str(format!("read key {key_path}: {e}").into()))),
    };
    Ok(ok(make_tls_config_value(cert, key)))
}

#[cfg(feature = "quic")]
fn dispatch_tls_self_signed(args: Vec<Value>) -> Result<Value, String> {
    let hostname = expect_str(args.first())?.to_string();
    match crate::quic::self_signed_pem(&hostname) {
        Ok((cert, key)) => Ok(ok(make_tls_config_value(cert, key))),
        Err(e) => Ok(err(Value::Str(format!("tls.self_signed: {e}").into()))),
    }
}

#[cfg(not(feature = "quic"))]
fn dispatch_tls_self_signed(_args: Vec<Value>) -> Result<Value, String> {
    Ok(err(Value::Str(
        "tls.self_signed: lex-runtime was compiled without the `quic` feature (needed for rcgen)".into(),
    )))
}

impl DefaultHandler {
    #[cfg(feature = "quic")]
    fn dispatch_serve_quic_named(&self, args: Vec<Value>) -> Result<Value, String> {
        let port = match args.first() {
            Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
            _ => return Err("net.serve_quic(port, tls, handler): port must be Int 0..=65535".into()),
        };
        let tls = decode_tls_config(args.get(1)
            .ok_or_else(|| "net.serve_quic(port, tls, handler): missing tls".to_string())?)?;
        let handler_name = expect_str(args.get(2))?.to_string();
        let program = self.program.clone()
            .ok_or_else(|| "net.serve_quic requires a Program reference; use DefaultHandler::with_program".to_string())?;
        let policy = self.policy.clone();
        crate::quic::serve_http3_named(port, handler_name, tls, program, policy, ServeOpts::from_env())
    }

    #[cfg(feature = "quic")]
    fn dispatch_serve_quic_fn(&self, args: Vec<Value>) -> Result<Value, String> {
        let port = match args.first() {
            Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
            _ => return Err("net.serve_quic_fn(port, tls, handler): port must be Int 0..=65535".into()),
        };
        let tls = decode_tls_config(args.get(1)
            .ok_or_else(|| "net.serve_quic_fn(port, tls, handler): missing tls".to_string())?)?;
        let closure = match args.into_iter().nth(2) {
            Some(c @ Value::Closure { .. }) => c,
            _ => return Err("net.serve_quic_fn(port, tls, handler): handler must be a closure".into()),
        };
        let program = self.program.clone()
            .ok_or_else(|| "net.serve_quic_fn requires a Program reference; use DefaultHandler::with_program".to_string())?;
        let policy = self.policy.clone();
        crate::quic::serve_http3_fn(port, closure, tls, program, policy, ServeOpts::from_env())
    }

    #[cfg(feature = "quic")]
    fn dispatch_serve_quic_routed(&self, args: Vec<Value>) -> Result<Value, String> {
        let port = match args.first() {
            Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
            _ => return Err("net.serve_quic_routed(port, tls, routes, fallback): port must be Int 0..=65535".into()),
        };
        let tls = decode_tls_config(args.get(1)
            .ok_or_else(|| "net.serve_quic_routed(port, tls, routes, fallback): missing tls".to_string())?)?;
        let routes_val = args.get(2).cloned()
            .ok_or_else(|| "net.serve_quic_routed(port, tls, routes, fallback): missing routes".to_string())?;
        let fallback = match args.into_iter().nth(3) {
            Some(c @ Value::Closure { .. }) => c,
            _ => return Err("net.serve_quic_routed(port, tls, routes, fallback): fallback must be a closure".into()),
        };
        let routes = decode_routes_arg(routes_val)?;
        let program = self.program.clone()
            .ok_or_else(|| "net.serve_quic_routed requires a Program reference; use DefaultHandler::with_program".to_string())?;
        let policy = self.policy.clone();
        crate::quic::serve_http3_routed(port, routes, fallback, tls, program, policy, ServeOpts::from_env())
    }

    #[cfg(not(feature = "quic"))]
    fn dispatch_serve_quic_named(&self, _args: Vec<Value>) -> Result<Value, String> {
        Err("net.serve_quic: lex-runtime was compiled without the `quic` feature (needed for quinn + h3)".into())
    }
    #[cfg(not(feature = "quic"))]
    fn dispatch_serve_quic_fn(&self, _args: Vec<Value>) -> Result<Value, String> {
        Err("net.serve_quic_fn: lex-runtime was compiled without the `quic` feature (needed for quinn + h3)".into())
    }
    #[cfg(not(feature = "quic"))]
    fn dispatch_serve_quic_routed(&self, _args: Vec<Value>) -> Result<Value, String> {
        Err("net.serve_quic_routed: lex-runtime was compiled without the `quic` feature (needed for quinn + h3)".into())
    }
}

/// Read `LEX_NET_HTTP2` and report whether the runtime should accept
/// HTTP/2 connections via hyper-util's auto builder (HTTP/1 ↔ HTTP/2
/// preface detection). Accepts `1` / `true` (case-insensitive); anything
/// else (including unset) keeps the HTTP/1-only default.
///
/// h2c (cleartext HTTP/2) needs prior-knowledge clients
/// (`curl --http2-prior-knowledge`, wrk/h2load, gRPC). Browsers do not
/// speak h2c — they require ALPN over TLS, which is a separate path.
/// See lex-lang#488.
fn env_http2() -> bool {
    match std::env::var("LEX_NET_HTTP2") {
        Ok(v) => {
            let s = v.trim().to_ascii_lowercase();
            s == "1" || s == "true"
        }
        Err(_) => false,
    }
}

/// Build a Lex request record from hyper request parts and pre-collected body bytes.
pub(crate) fn build_request_value_parts(
    parts: &hyper::http::request::Parts,
    body: &bytes::Bytes,
) -> Value {
    let method = parts.method.as_str().to_string();
    // `Uri::path()` returns just the origin-form path, regardless of
    // whether the wire URI was relative (`/foo` — HTTP/1.1) or
    // absolute (`https://host/foo` — HTTP/2 and HTTP/3 fold the
    // `:scheme` + `:authority` pseudo-headers into the full URI).
    // Reading `to_string()` would leak the scheme/authority into the
    // Lex handler's `req.path`, which surprised handlers built for
    // HTTP/1.1 (#496 surfaced this against `serve_quic`).
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string).unwrap_or_default();
    let mut headers_map = std::collections::BTreeMap::new();
    for (name, val) in &parts.headers {
        if let Ok(v) = val.to_str() {
            headers_map.insert(
                lex_bytecode::MapKey::Str(name.as_str().to_ascii_lowercase()),
                Value::Str(v.to_string().into()),
            );
        }
    }
    let body_str = String::from_utf8_lossy(body).into_owned();
    let mut rec = indexmap::IndexMap::new();
    rec.insert("method".into(), Value::Str(method.into()));
    rec.insert("path".into(), Value::Str(path.into()));
    rec.insert("query".into(), Value::Str(query.into()));
    rec.insert("body".into(), Value::Str(body_str.into()));
    rec.insert("headers".into(), Value::Map(headers_map));
    rec.insert("path_params".into(), Value::Map(std::collections::BTreeMap::new()));
    Value::record_dynamic(rec)
}

/// Build a Lex request record from a tiny_http request (used by the TLS path).
fn build_request_value_tiny(req: &mut tiny_http::Request) -> Value {
    let method = format!("{:?}", req.method()).to_uppercase();
    let url = req.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url, String::new()),
    };
    let mut headers_map = std::collections::BTreeMap::new();
    for h in req.headers() {
        headers_map.insert(
            lex_bytecode::MapKey::Str(h.field.as_str().as_str().to_ascii_lowercase()),
            Value::Str(h.value.as_str().to_string().into()),
        );
    }
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let mut rec = indexmap::IndexMap::new();
    rec.insert("method".into(), Value::Str(method.into()));
    rec.insert("path".into(), Value::Str(path.into()));
    rec.insert("query".into(), Value::Str(query.into()));
    rec.insert("body".into(), Value::Str(body.into()));
    rec.insert("headers".into(), Value::Map(headers_map));
    rec.insert("path_params".into(), Value::Map(std::collections::BTreeMap::new()));
    Value::record_dynamic(rec)
}

pub(crate) fn unpack_response(vm: &mut Vm, v: &Value) -> UnpackedResponse {
    // Accept both heap `Record` and arena `ArenaRecord` — the new
    // slab-direct accessors below read each uniformly without
    // requiring a tree-wide materialize first. See
    // `docs/design/arena-plumbing.md` § "Status update (2026-06-05)"
    // for the wire-up rationale.
    if !matches!(v, Value::Record { .. } | Value::ArenaRecord { .. }) {
        return (
            500,
            ResponseBodyOut::Str(format!("handler returned non-record: {v:?}")),
            vec![],
        );
    }

    let status = vm.get_record_field(v, "status").and_then(|s| match s {
        Value::Int(n) => Some(n as u16),
        _ => None,
    }).unwrap_or(200);

    // Body — read once, drain lazy iters inline so the VM is still
    // in scope when `materialize_lazy_iter` runs. Replaces the
    // previously-separate `materialize_response_body` pass.
    let body = match vm.get_record_field(v, "body") {
        Some(Value::Variant { name, mut args }) if args.len() == 1 => {
            let inner = args.pop().unwrap();
            match (name.as_str(), inner) {
                // Tagged ResponseBody (#375): BodyStr | BodyStream | BodyBytes.
                ("BodyStr", Value::Str(s)) => ResponseBodyOut::Str(s.to_string()),
                ("BodyStream", iter_v) => {
                    let drained = materialize_lazy_iter(vm, iter_v);
                    ResponseBodyOut::TextChunks(drain_iter_str(&drained))
                }
                ("BodyBytes", iter_v) => {
                    let drained = materialize_lazy_iter(vm, iter_v);
                    ResponseBodyOut::BytesChunks(drain_iter_bytes(&drained))
                }
                _ => ResponseBodyOut::Str(String::new()),
            }
        }
        // Escape hatch for handlers that don't use the nominal
        // `Response` alias and just return a structural record with
        // `body :: Str` (the pre-#375 contract). Lets internal
        // test handlers and one-liners keep working without
        // wrapping in `BodyStr(...)`.
        Some(Value::Str(s)) => ResponseBodyOut::Str(s.to_string()),
        _ => ResponseBodyOut::Str(String::new()),
    };

    let headers: Vec<(String, String)> = match vm.get_record_field(v, "headers") {
        Some(Value::Map(hmap)) => hmap.iter().filter_map(|(k, val)| {
            if let (lex_bytecode::MapKey::Str(name), Value::Str(s)) = (k, val) {
                Some((name.clone(), s.to_string()))
            } else {
                None
            }
        }).collect(),
        _ => vec![],
    };

    (status, body, headers)
}

type HyperRespBody =
    http_body_util::combinators::BoxBody<bytes::Bytes, std::convert::Infallible>;

/// Build a hyper response from the unpacked handler tuple
/// `(status, body, headers)`. The `unpack_response` step runs inside
/// the spawn_blocking closure (where `vm` is still alive) so this
/// function doesn't need `&Vm` — arena handles, lazy iters, and the
/// like are already resolved by the time we get here. Streaming
/// bodies (`BodyStream`, `BodyBytes`) use `ChunkedBody` which has no
/// known `size_hint`, so hyper emits `Transfer-Encoding: chunked` on
/// the wire. Plain string bodies use `Full<Bytes>` which carries
/// `Content-Length`.
fn build_hyper_response(
    (status, body, headers): UnpackedResponse,
) -> hyper::Response<HyperRespBody> {
    use http_body_util::BodyExt as _;
    let boxed_body: HyperRespBody = match body {
        ResponseBodyOut::Str(s) => {
            http_body_util::Full::new(bytes::Bytes::from(s.into_bytes())).boxed()
        }
        ResponseBodyOut::TextChunks(chunks) | ResponseBodyOut::BytesChunks(chunks) => {
            HyperChunkedBody::from(chunks).boxed()
        }
    };
    let mut builder = hyper::Response::builder().status(status);
    for (name, val) in headers {
        builder = builder.header(name, val);
    }
    builder
        .body(boxed_body)
        .unwrap_or_else(|_| error_response(500, "response build error"))
}

fn error_response(status: u16, msg: &str) -> hyper::Response<HyperRespBody> {
    use http_body_util::BodyExt as _;
    hyper::Response::builder()
        .status(status)
        .body(
            http_body_util::Full::new(bytes::Bytes::from(msg.to_owned()))
                .boxed(),
        )
        .unwrap_or_else(|_| {
            use http_body_util::BodyExt as _;
            hyper::Response::new(http_body_util::Empty::new().map_err(|e| match e {}).boxed())
        })
}

/// Async body that emits pre-collected chunks as separate HTTP frames, causing
/// hyper to use `Transfer-Encoding: chunked` (no `size_hint` exact count).
struct HyperChunkedBody {
    chunks: std::collections::VecDeque<Vec<u8>>,
}

impl From<Vec<Vec<u8>>> for HyperChunkedBody {
    fn from(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks: chunks.into_iter().filter(|c| !c.is_empty()).collect(),
        }
    }
}

impl hyper::body::Body for HyperChunkedBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match self.chunks.pop_front() {
            Some(chunk) => std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(
                bytes::Bytes::from(chunk),
            )))),
            None => std::task::Poll::Ready(None),
        }
    }
}

/// Send `body` back on a TLS `tiny_http` request. Used only by the
/// `net.serve_tls` path which still runs on tiny_http pending a
/// tokio-rustls migration.
fn respond_with_body_tls(
    req: tiny_http::Request,
    status: u16,
    body: ResponseBodyOut,
    headers: Vec<(String, String)>,
) {
    let tiny_headers: Vec<tiny_http::Header> = headers
        .into_iter()
        .filter_map(|(name, val)| format!("{name}: {val}").parse::<tiny_http::Header>().ok())
        .collect();
    match body {
        ResponseBodyOut::Str(s) => {
            let mut response = tiny_http::Response::from_string(s).with_status_code(status);
            for h in tiny_headers {
                response.add_header(h);
            }
            let _ = req.respond(response);
        }
        ResponseBodyOut::TextChunks(chunks) | ResponseBodyOut::BytesChunks(chunks) => {
            let reader = ChunkReader::new(chunks);
            let response = tiny_http::Response::new(
                tiny_http::StatusCode(status),
                tiny_headers,
                reader,
                None,
                None,
            );
            let _ = req.respond(response);
        }
    }
}

/// Decoded `Response.body` (#375). The runtime emits each variant via a
/// different `tiny_http` path: a single `Response::from_string` for
/// `Str`, and a chunked-encoding `Response::new` with a `Read`-backed
/// chunk list for the streaming variants.
///
/// The shape `unpack_response` returns: `(status_code, body, headers)`.
/// Factored out as a `type` alias so call sites that store it (the
/// spawn_blocking closures' `Result<UnpackedResponse, ...>`) don't
/// trip clippy's `type_complexity` lint.
pub(crate) type UnpackedResponse = (u16, ResponseBodyOut, Vec<(String, String)>);

pub(crate) enum ResponseBodyOut {
    Str(String),
    /// Pre-drained text chunks. v1 ships eager-iter only; lazy producers
    /// (#376 follow-up) will replace this with a Read adapter that pulls
    /// chunks on demand from the VM.
    TextChunks(Vec<Vec<u8>>),
    /// Pre-drained binary chunks. Each inner `Vec<u8>` is one Lex
    /// `List[Int]` collapsed down to a byte vector.
    BytesChunks(Vec<Vec<u8>>),
}

/// Walk a Lex `Iter[Str]` (eager (List, Int) representation) and produce
/// a chunk list. The chunks are byte vectors so the chunked-Read adapter
/// is uniform across text and binary streams.
///
/// Iter[T] representation shifted in #376: from `Tuple([list, idx])` to
/// `Variant("__IterEager", [list, idx])` for the eager form. Lazy iters
/// produced by `iter.unfold` (`Variant("__IterLazy", [seed, step])`) and
/// cursor-backed iters (`Variant("__IterCursor", [handle])` from #379)
/// are not drained eagerly here — the v1 streaming path covers only the
/// eager form. Lazy/cursor producers will be wired through the
/// `ChunkReader` in a follow-up so each `read()` calls `iter.next` via
/// the VM, preserving wall-clock chunk boundaries on the wire.
fn drain_iter_str(v: &Value) -> Vec<Vec<u8>> {
    match v {
        Value::Variant { name, args }
            if name == "__IterEager" && args.len() == 2 =>
        {
            if let (Value::List(items), Value::Int(idx)) = (&args[0], &args[1]) {
                items.iter().skip(*idx as usize).filter_map(|item| {
                    if let Value::Str(s) = item { Some(s.as_bytes().to_vec()) } else { None }
                }).collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Walk a Lex `Iter[List[Int]]` and produce a chunk list. Each `List[Int]`
/// element is collapsed by truncating each Int to u8 (0..=255). See
/// `drain_iter_str` for the lazy/cursor-iter limitation.
fn drain_iter_bytes(v: &Value) -> Vec<Vec<u8>> {
    match v {
        Value::Variant { name, args }
            if name == "__IterEager" && args.len() == 2 =>
        {
            if let (Value::List(items), Value::Int(idx)) = (&args[0], &args[1]) {
                items.iter().skip(*idx as usize).filter_map(|item| {
                    if let Value::List(ints) = item {
                        Some(ints.iter().filter_map(|i| match i {
                            Value::Int(n) => Some((*n & 0xff) as u8),
                            _ => None,
                        }).collect::<Vec<u8>>())
                    } else {
                        None
                    }
                }).collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Drive an `__IterLazy(seed, step)` to exhaustion by invoking the step
/// closure via `vm`, then return an equivalent `__IterEager(list, 0)` so
/// the existing `drain_iter_*` paths can consume it.
///
/// Without this pre-pass, `BodyStream(iter.unfold(...))` produces empty
/// response bodies because the drain helpers match only on the eager
/// variant (#477). The step closure can carry effects; we ignore that
/// here — the handler is already running on a tokio task with the same
/// effect bindings, so any `[net]` / `[time]` calls inside the step
/// re-enter the same handler context.
///
/// `__IterEager` is returned untouched. Unknown variants pass through.
fn materialize_lazy_iter(vm: &mut Vm, v: Value) -> Value {
    let mut current = v;
    let mut items: Vec<Value> = Vec::new();
    loop {
        match current {
            Value::Variant { name, args } if name == "__IterLazy" && args.len() == 2 => {
                let seed = args[0].clone();
                let step = args[1].clone();
                match vm.invoke_closure_value(step.clone(), vec![seed]) {
                    Ok(Value::Variant { name: opt, args: opt_args })
                        if opt == "None" =>
                    {
                        let _ = opt_args;
                        break;
                    }
                    Ok(Value::Variant { name: opt, args: opt_args })
                        if opt == "Some" && opt_args.len() == 1 =>
                    {
                        if let Value::Tuple(pair) = &opt_args[0] {
                            if pair.len() == 2 {
                                items.push(pair[0].clone());
                                current = Value::Variant {
                                    name: "__IterLazy".to_string(),
                                    args: vec![pair[1].clone(), step],
                                };
                                continue;
                            }
                        }
                        // Malformed pair — bail to avoid infinite loop.
                        break;
                    }
                    _ => break,
                }
            }
            // Already eager (or unknown) — return as-is, possibly with
            // any items we collected from a partial drain.
            other => {
                if items.is_empty() {
                    return other;
                }
                // Mixed shape shouldn't happen in practice; fall through
                // to the eager builder below with the items we have.
                let _ = other;
                break;
            }
        }
    }
    Value::Variant {
        name: "__IterEager".to_string(),
        args: vec![
            Value::List(items.into_iter().collect()),
            Value::Int(0),
        ],
    }
}


/// `Read` adapter that returns one Lex chunk per `read()` call so
/// `tiny_http`'s chunked transfer-encoding emits each Lex chunk as a
/// distinct HTTP chunk on the wire. When the requested buffer is smaller
/// than the current chunk we serve a slice and keep the remainder for
/// the next call.
struct ChunkReader {
    chunks: std::collections::VecDeque<Vec<u8>>,
    cursor: usize,
}

impl ChunkReader {
    fn new(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks: chunks.into_iter().filter(|c| !c.is_empty()).collect(),
            cursor: 0,
        }
    }
}

impl std::io::Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let Some(front) = self.chunks.front() else {
                return Ok(0);
            };
            let remaining = &front[self.cursor..];
            if remaining.is_empty() {
                self.chunks.pop_front();
                self.cursor = 0;
                continue;
            }
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.cursor += n;
            if self.cursor >= front.len() {
                self.chunks.pop_front();
                self.cursor = 0;
            }
            return Ok(n);
        }
    }
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
                Value::Variant { name: "Ok".into(), args: vec![Value::Str(body.into())] }
            } else {
                err_value(format!("status {status}: {body}"))
            }
        }
        Err(e) => err_value(format!("transport: {e}")),
    }
}

/// Build a ureq agent for `http.stream_lines` with a long timeout.
/// Local models (Ollama, vLLM) can take minutes to load before they start
/// responding, and thinking-heavy models can take minutes to finish.
/// Use timeout_global so the limit applies to the entire operation
/// (connect + send + recv) rather than individual phases, avoiding the
/// 10-second default that with_config().read_to_vec() uses for body reads.
fn http_stream_agent() -> ureq::Agent {
    use std::time::Duration;
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Build a ureq agent for `std.http.{send,get,post}` with the given
/// timeout (None → use the same defaults as the legacy `net.{get,post}`
/// path). Separate from `http_request` so the rich `http.send` flow
/// can supply per-request overrides.
///
/// When the caller supplies `timeout_ms` we apply it as a single
/// `timeout_global` covering the whole operation (connect + send + recv)
/// and drop the per-phase caps — exactly like `http_stream_agent`. A
/// per-phase cap (notably the bound on waiting for the *first* response
/// byte) would otherwise fire long before the caller's budget: a slow
/// first response — e.g. an LLM cold-loading a multi-GB model — then
/// fails at ~10s even though `timeout_ms` was set to 120000. (#646)
fn http_agent(timeout_ms: Option<u64>) -> ureq::Agent {
    use std::time::Duration;
    match timeout_ms {
        Some(ms) => ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(ms)))
            .http_status_as_error(false)
            .build()
            .into(),
        None => ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_body(Some(Duration::from_secs(30)))
            .timeout_send_body(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .build()
            .into(),
    }
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
    let args = match payload { Some(s) => vec![Value::Str(s.into())], None => vec![] };
    let inner = Value::Variant { name: ctor.into(), args };
    Value::Variant { name: "Err".into(), args: vec![inner] }
}

fn http_decode_err(msg: String) -> Value {
    let inner = Value::Variant {
        name: "DecodeError".into(),
        args: vec![Value::Str(msg.into())],
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
    // Normalise method to uppercase before matching. Per RFC 7230, HTTP
    // methods are case-sensitive, but lex callers naturally write
    // `"put"` / `"PUT"` interchangeably; uppercasing here keeps the
    // surface forgiving without compromising the wire format (ureq
    // sends whatever method name we pass to the per-method builder).
    let method_upper = method.to_ascii_uppercase();
    let body_bytes: Vec<u8> = body.unwrap_or_default();
    let resp = match method_upper.as_str() {
        // Bodyless methods. PUT/PATCH/DELETE technically allow a body,
        // but in practice (and per #503's OCPI flows) DELETE is most
        // often bodyless; if a future caller needs DELETE-with-body
        // we can split it via a different ureq builder.
        "GET" => {
            let mut req = agent.get(url);
            if !content_type.is_empty() { req = req.header("content-type", content_type); }
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.call()
        }
        "HEAD" => {
            let mut req = agent.head(url);
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.call()
        }
        "DELETE" => {
            let mut req = agent.delete(url);
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.call()
        }
        // Methods that carry a request body. `body.unwrap_or_default()`
        // means a missing body sends an empty payload, which is the
        // correct default for POST `{}` style requests and matches
        // curl's `-X POST` (no `-d`) behaviour.
        "POST" => {
            let mut req = agent.post(url);
            if !content_type.is_empty() { req = req.header("content-type", content_type); }
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.send(&body_bytes[..])
        }
        "PUT" => {
            let mut req = agent.put(url);
            if !content_type.is_empty() { req = req.header("content-type", content_type); }
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.send(&body_bytes[..])
        }
        "PATCH" => {
            let mut req = agent.patch(url);
            if !content_type.is_empty() { req = req.header("content-type", content_type); }
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.send(&body_bytes[..])
        }
        m => {
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
            Value::Variant { name: "Ok".into(), args: vec![Value::record_dynamic(rec)] }
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
        out.insert(lex_bytecode::MapKey::Str(name.as_str().to_string()), Value::Str(v.into()));
    }
    out
}

/// Pull the standard `HttpRequest` shape out of a `Value::Record`
/// and dispatch through `http_send_full`. The handler verifies
/// `--allow-net-host` for the URL before sending.
fn http_send_record(handler: &DefaultHandler, req: &indexmap::IndexMap<smol_str::SmolStr, Value>) -> Value {
    let method = match req.get("method") {
        Some(Value::Str(s)) => s.to_string(),
        _ => return http_decode_err("HttpRequest.method must be Str".into()),
    };
    let url = match req.get("url") {
        Some(Value::Str(s)) => s.to_string(),
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
            let vv = match v { Value::Str(s) => s.to_string(), _ => return None };
            Some((kk, vv))
        }).collect(),
        _ => return http_decode_err("HttpRequest.headers must be Map[Str, Str]".into()),
    };
    http_send_full(&method, &url, body, "", &headers, timeout_ms)
}

fn expect_record(v: Option<&Value>) -> Result<&indexmap::IndexMap<smol_str::SmolStr, Value>, String> {
    match v {
        Some(Value::Record { fields: r, .. }) => Ok(r),
        Some(other) => Err(format!("expected Record, got {other:?}")),
        None => Err("missing Record argument".into()),
    }
}

fn err_value(msg: String) -> Value {
    Value::Variant { name: "Err".into(), args: vec![Value::Str(msg.into())] }
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

/// HTTP POST that buffers the full response body then yields it split into lines.
/// Intended for LLM provider APIs (OpenAI, Anthropic, Google) that use SSE/NDJSON
/// and close the connection after sending all events. Connection errors → `Err(Str)`.
///
/// CAUTION: ureq 3.3's `Body` does not implement `std::io::Read` and exposes no
/// incremental read API. The entire response is buffered via `read_to_vec()` before
/// splitting. This means the call blocks until the server closes the connection —
/// endpoints that hold the connection open indefinitely will hang. True per-line
/// streaming requires a future HTTP client upgrade.
fn http_stream_lines_impl(_handler: &DefaultHandler, url: &str, headers_val: &Value, body: &str) -> Value {
    let body_bytes = body.as_bytes().to_vec();
    // Use a 10-minute body-read timeout — local models (Ollama, vLLM) can take
    // several minutes to generate long thinking traces before closing the stream.
    let agent = http_stream_agent();
    let mut req = agent.post(url);
    if let Value::Map(headers) = headers_val {
        for (k, v) in headers {
            let key_str = match k {
                lex_bytecode::MapKey::Str(s) => s.as_str(),
                _ => continue,
            };
            if let Value::Str(val) = v {
                req = req.header(key_str, val.as_str());
            }
        }
    }
    match req.send(&body_bytes[..]) {
        Ok(resp) => {
            let bytes = match resp.into_body().with_config().read_to_vec() {
                Ok(b) => b,
                Err(e) => return err(Value::Str(format!("http.stream_lines: body read: {e}").into())),
            };
            let raw_text = String::from_utf8_lossy(&bytes).into_owned();
            let text = decode_unicode_escapes(&raw_text);
            let items: std::collections::VecDeque<Value> = text.lines()
                .map(|l| Value::Str(l.to_string().into()))
                .collect();
            let iter_val = Value::Variant {
                name: "__IterEager".into(),
                args: vec![Value::List(items), Value::Int(0)],
            };
            ok(iter_val)
        }
        Err(e) => err(Value::Str(format!("http.stream_lines: {e}").into())),
    }
}

fn decode_unicode_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        match chars.peek() {
            Some('u') => {
                chars.next();
                let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                if hex.len() == 4 {
                    if let Ok(n) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(n) {
                            result.push(ch);
                            continue;
                        }
                    }
                }
                result.push('\\');
                result.push('u');
                result.push_str(&hex);
            }
            _ => result.push(c),
        }
    }
    result
}

/// Build a `SqlError = { message, code, detail }` Lex record (#380).
/// `code` and `detail` are `None` by default; the driver-specific
/// converters below populate them with real values.
fn sql_error(message: impl Into<String>, code: Option<String>, detail: Option<String>) -> Value {
    let some = |s: String| Value::Variant { name: "Some".into(), args: vec![Value::Str(s.into())] };
    let none = || Value::Variant { name: "None".into(), args: vec![] };
    let mut rec = indexmap::IndexMap::new();
    let msg: String = message.into();
    rec.insert("message".into(), Value::Str(msg.into()));
    rec.insert("code".into(), match code {
        Some(c) => some(c),
        None => none(),
    });
    rec.insert("detail".into(), match detail {
        Some(d) => some(d),
        None => none(),
    });
    Value::record_dynamic(rec)
}

/// Convert a rusqlite error into a `SqlError`. The `code` is the
/// symbolic extended-result-code name (`SQLITE_BUSY`,
/// `SQLITE_CONSTRAINT_UNIQUE`, …) when present — this is what
/// callers want for dialect-aware retry / conflict handling.
///
/// rusqlite has two main error shapes that carry a numeric code:
/// `SqliteFailure` (driver-side runtime errors — constraints, busy,
/// IO) and `SqlInputError` (statement-preparation failures —
/// syntax, unknown table). Both are unpacked the same way.
fn sqlite_err_to_sql_error(e: rusqlite::Error, op: &str) -> Value {
    let message = format!("{op}: {e}");
    match &e {
        rusqlite::Error::SqliteFailure(ffi, detail_opt) => {
            sql_error(
                message,
                Some(sqlite_extended_code_name(ffi.extended_code)),
                detail_opt.clone(),
            )
        }
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            sql_error(
                message,
                Some(sqlite_extended_code_name(error.extended_code)),
                Some(msg.clone()),
            )
        }
        _ => sql_error(message, None, None),
    }
}

/// Map a SQLite extended result code (numeric) to its symbolic name.
/// We only cover the codes a Lex caller is likely to dispatch on
/// (constraint kinds, busy/locked, read-only, IO); anything else
/// falls back to a generic `SQLITE_ERROR_<n>` stringification so the
/// numeric code is still recoverable.
fn sqlite_extended_code_name(code: i32) -> String {
    use rusqlite::ffi::*;
    let s = match code {
        SQLITE_BUSY => "SQLITE_BUSY",
        SQLITE_LOCKED => "SQLITE_LOCKED",
        SQLITE_READONLY => "SQLITE_READONLY",
        SQLITE_IOERR => "SQLITE_IOERR",
        SQLITE_CORRUPT => "SQLITE_CORRUPT",
        SQLITE_NOTFOUND => "SQLITE_NOTFOUND",
        SQLITE_FULL => "SQLITE_FULL",
        SQLITE_CANTOPEN => "SQLITE_CANTOPEN",
        SQLITE_PROTOCOL => "SQLITE_PROTOCOL",
        SQLITE_SCHEMA => "SQLITE_SCHEMA",
        SQLITE_TOOBIG => "SQLITE_TOOBIG",
        SQLITE_CONSTRAINT => "SQLITE_CONSTRAINT",
        SQLITE_CONSTRAINT_CHECK => "SQLITE_CONSTRAINT_CHECK",
        SQLITE_CONSTRAINT_FOREIGNKEY => "SQLITE_CONSTRAINT_FOREIGNKEY",
        SQLITE_CONSTRAINT_NOTNULL => "SQLITE_CONSTRAINT_NOTNULL",
        SQLITE_CONSTRAINT_PRIMARYKEY => "SQLITE_CONSTRAINT_PRIMARYKEY",
        SQLITE_CONSTRAINT_TRIGGER => "SQLITE_CONSTRAINT_TRIGGER",
        SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE",
        SQLITE_CONSTRAINT_VTAB => "SQLITE_CONSTRAINT_VTAB",
        SQLITE_CONSTRAINT_ROWID => "SQLITE_CONSTRAINT_ROWID",
        SQLITE_MISMATCH => "SQLITE_MISMATCH",
        SQLITE_RANGE => "SQLITE_RANGE",
        SQLITE_NOTADB => "SQLITE_NOTADB",
        SQLITE_AUTH => "SQLITE_AUTH",
        _ => return format!("SQLITE_ERROR_{code}"),
    };
    s.to_string()
}

/// Convert a postgres error into a `SqlError`. The `code` is the
/// 5-character SQLSTATE (`23505`, `40P01`, …); `detail` is the
/// driver's optional detail message when present.
fn pg_err_to_sql_error(e: postgres::Error, op: &str) -> Value {
    let message = format!("{op}: {e}");
    let code = e.as_db_error().map(|db| db.code().code().to_string());
    let detail = e.as_db_error().and_then(|db| db.detail().map(|s| s.to_string()));
    sql_error(message, code, detail)
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
                "agent.call_mcp: args_json is not valid JSON: {e}").into())),
        };
        match self.mcp_clients.call(&server, &tool, parsed) {
            Ok(result) => ok(Value::Str(
                serde_json::to_string(&result).unwrap_or_else(|_| "null".into()).into())),
            Err(e) => err(Value::Str(e.into())),
        }
    }

    /// Implementation of `agent.cloud_stream(prompt) -> Result[Stream[Str], Str]`
    /// (#305 slice 3). The fixture path (`LEX_LLM_STREAM_FIXTURE`)
    /// splits the env-var value on `|` and yields each segment as
    /// one chunk; it's the load-bearing test hook. Live HTTP
    /// chunked-response support is deferred to a follow-up slice.
    fn dispatch_cloud_stream(&mut self, args: Vec<Value>) -> Value {
        let _prompt = match args.first() {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.cloud_stream(prompt): prompt must be Str".into())),
        };
        let chunks: Vec<String> = match std::env::var("LEX_LLM_STREAM_FIXTURE") {
            Ok(v) => v.split('|').map(|s| s.to_string()).collect(),
            Err(_) => return err(Value::Str(
                "agent.cloud_stream: live streaming not yet implemented; \
                 set LEX_LLM_STREAM_FIXTURE='chunk1|chunk2|…' for tests".into())),
        };
        let handle = self.register_stream(chunks.into_iter());
        ok(stream_handle_value(handle))
    }

    /// Implementation of `stream.next(s) -> Option[T]` (#305 slice 3).
    /// Returns `Some(chunk)` for each producer yield and `None` once
    /// the producer is exhausted. Unknown handle ids return `None`
    /// rather than erroring so streams can be safely consumed past
    /// the end (matches the semantics of `Iterator::next`).
    fn dispatch_stream_next(&mut self, args: Vec<Value>) -> Value {
        let handle = match args.first().and_then(stream_handle_id) {
            Some(h) => h,
            None => return Value::Variant { name: "None".into(), args: vec![] },
        };
        let mut streams = match self.streams.lock() {
            Ok(g) => g,
            Err(_) => return Value::Variant { name: "None".into(), args: vec![] },
        };
        match streams.get_mut(&handle).and_then(|it| it.next()) {
            Some(chunk) => some(Value::Str(chunk.into())),
            None => {
                streams.remove(&handle);
                Value::Variant { name: "None".into(), args: vec![] }
            }
        }
    }

    /// Implementation of `stream.collect(s) -> List[T]` (#305 slice 3).
    /// Drains the producer eagerly. Unknown handles drain to an
    /// empty list so the contract is `collect ∘ collect = []`
    /// (idempotent on a closed stream).
    fn dispatch_stream_collect(&mut self, args: Vec<Value>) -> Value {
        let handle = match args.first().and_then(stream_handle_id) {
            Some(h) => h,
            None => return Value::List(std::collections::VecDeque::new()),
        };
        let mut iter = {
            let mut streams = match self.streams.lock() {
                Ok(g) => g,
                Err(_) => return Value::List(std::collections::VecDeque::new()),
            };
            match streams.remove(&handle) {
                Some(it) => it,
                None => return Value::List(std::collections::VecDeque::new()),
            }
        };
        let mut out: std::collections::VecDeque<Value> = std::collections::VecDeque::new();
        for chunk in iter.by_ref() {
            out.push_back(Value::Str(chunk.into()));
        }
        Value::List(out)
    }

    /// Register a producer iterator and return its handle id. The
    /// handle is monotonic-counter-based so two streams created in
    /// quick succession get distinct ids.
    fn register_stream<I>(&self, iter: I) -> String
    where
        I: Iterator<Item = String> + Send + 'static,
    {
        let id = self
            .next_stream_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let handle = format!("stream_{id}");
        if let Ok(mut streams) = self.streams.lock() {
            streams.insert(handle.clone(), Box::new(iter));
        }
        handle
    }
}

/// Build the runtime representation of a `Stream[T]` value:
/// `Variant("__StreamHandle", [Str(handle_id)])`. The opaque tag is
/// prefixed with `__` so it can't collide with a user-declared
/// variant.
fn stream_handle_value(handle: String) -> Value {
    Value::Variant {
        name: "__StreamHandle".into(),
        args: vec![Value::Str(handle.into())],
    }
}

/// Inverse of [`stream_handle_value`] — extract the handle id from
/// a Stream value, or `None` if the input doesn't have the
/// expected shape.
fn stream_handle_id(v: &Value) -> Option<String> {
    match v {
        Value::Variant { name, args } if name == "__StreamHandle" => match args.first() {
            Some(Value::Str(h)) => Some(h.to_string()),
            _ => None,
        },
        _ => None,
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
        Ok(text) => ok(Value::Str(text.into())),
        Err(e) => err(Value::Str(e.into())),
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
        Ok(text) => ok(Value::Str(text.into())),
        Err(e) => err(Value::Str(e.into())),
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

#[allow(dead_code)]
fn expect_str_list(v: Option<&Value>) -> Result<Vec<String>, String> {
    match v {
        Some(Value::List(items)) => items.iter().map(|x| match x {
            Value::Str(s) => Ok(s.to_string()),
            other => Err(format!("expected List[Str] element, got {other:?}")),
        }).collect(),
        Some(other) => Err(format!("expected List[Str], got {other:?}")),
        None => Err("missing List[Str] argument".into()),
    }
}

/// Convert a `List[SqlParam]` value to driver-neutral `SqlParamValue`s.
/// SqlParam = PStr(Str) | PInt(Int) | PFloat(Float) | PBool(Bool) | PNull
fn expect_sql_params(v: Option<&Value>) -> Result<Vec<SqlParamValue>, String> {
    let items = match v {
        Some(Value::List(xs)) => xs,
        Some(other) => return Err(format!("expected List[SqlParam], got {other:?}")),
        None => return Err("missing params argument".into()),
    };
    items.iter().map(|item| {
        match item {
            Value::Variant { name, args } => match name.as_str() {
                "PStr"   => match args.first() {
                    Some(Value::Str(s)) => Ok(SqlParamValue::Text(s.to_string())),
                    _ => Err("PStr requires a Str argument".into()),
                },
                "PInt"   => match args.first() {
                    Some(Value::Int(n)) => Ok(SqlParamValue::Integer(*n)),
                    _ => Err("PInt requires an Int argument".into()),
                },
                "PFloat" => match args.first() {
                    Some(Value::Float(f)) => Ok(SqlParamValue::Real(*f)),
                    _ => Err("PFloat requires a Float argument".into()),
                },
                "PBool"  => match args.first() {
                    Some(Value::Bool(b)) => Ok(SqlParamValue::Bool(*b)),
                    _ => Err("PBool requires a Bool argument".into()),
                },
                "PNull"  => Ok(SqlParamValue::Null),
                other    => Err(format!("unknown SqlParam constructor `{other}`")),
            },
            // Backward-compat: bare strings are accepted as PStr.
            Value::Str(s) => Ok(SqlParamValue::Text(s.to_string())),
            other => Err(format!("expected SqlParam variant, got {other:?}")),
        }
    }).collect()
}

/// Convert `SqlParamValue`s to rusqlite-typed values for SQLite binding.
fn sqlite_params(params: &[SqlParamValue]) -> Vec<rusqlite::types::Value> {
    params.iter().map(|p| match p {
        SqlParamValue::Text(s)    => rusqlite::types::Value::Text(s.clone()),
        SqlParamValue::Integer(n) => rusqlite::types::Value::Integer(*n),
        SqlParamValue::Real(f)    => rusqlite::types::Value::Real(*f),
        SqlParamValue::Bool(b)    => rusqlite::types::Value::Integer(*b as i64),
        SqlParamValue::Null       => rusqlite::types::Value::Null,
    }).collect()
}

/// Box `SqlParamValue`s as `dyn ToSql + Sync` for Postgres binding.
fn pg_param_refs(params: &[SqlParamValue]) -> Vec<Box<dyn postgres::types::ToSql + Sync>> {
    params.iter().map(|p| -> Box<dyn postgres::types::ToSql + Sync> {
        match p {
            SqlParamValue::Text(s)    => Box::new(s.clone()),
            SqlParamValue::Integer(n) => Box::new(*n),
            SqlParamValue::Real(f)    => Box::new(*f),
            SqlParamValue::Bool(b)    => Box::new(*b),
            SqlParamValue::Null       => Box::new(Option::<String>::None),
        }
    }).collect()
}

/// Run a statement on SQLite and pack rows into `Value::List(Value::Record(...))`.
fn sql_run_query_sqlite(
    conn: &rusqlite::Connection,
    stmt_str: &str,
    params: &[SqlParamValue],
) -> Value {
    let mut stmt = match conn.prepare(stmt_str) {
        Ok(s)  => s,
        Err(e) => return err(sqlite_err_to_sql_error(e, "sql.query")),
    };
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let bound = sqlite_params(params);
    let bind: Vec<&dyn rusqlite::ToSql> = bound.iter()
        .map(|p| p as &dyn rusqlite::ToSql)
        .collect();
    let mut rows = match stmt.query(rusqlite::params_from_iter(bind.iter())) {
        Ok(r)  => r,
        Err(e) => return err(sqlite_err_to_sql_error(e, "sql.query")),
    };
    let mut out: Vec<Value> = Vec::new();
    loop {
        let row = match rows.next() {
            Ok(Some(r)) => r,
            Ok(None)    => break,
            Err(e)      => return err(sqlite_err_to_sql_error(e, "sql.query")),
        };
        let mut rec = indexmap::IndexMap::new();
        for (i, name) in column_names.iter().enumerate() {
            let cell = match row.get_ref(i) {
                Ok(c)  => sql_value_ref_to_lex(c),
                Err(e) => return err(sqlite_err_to_sql_error(e, &format!("sql.query: column {i}"))),
            };
            rec.insert(name.clone(), cell);
        }
        out.push(Value::record_dynamic(rec));
    }
    ok(Value::List(out.into()))
}

/// Run a statement on Postgres and pack rows into `Value::List(Value::Record(...))`.
fn sql_run_query_pg(
    client: &mut postgres::Client,
    stmt_str: &str,
    params: &[SqlParamValue],
) -> Value {
    let pg = pg_param_refs(params);
    let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
        pg.iter().map(|b| b.as_ref()).collect();
    let rows = match client.query(stmt_str, &refs) {
        Ok(r)  => r,
        Err(e) => return err(pg_err_to_sql_error(e, "sql.query")),
    };
    let out: std::collections::VecDeque<Value> = rows.iter().map(|row| {
        Value::record_dynamic(pg_row_to_lex_record(row))
    }).collect();
    ok(Value::List(out))
}

/// Convert a Postgres row to a Lex record, mapping column types to Lex values.
fn pg_row_to_lex_record(row: &postgres::Row) -> indexmap::IndexMap<String, Value> {
    use postgres::types::Type;
    let mut rec = indexmap::IndexMap::new();
    for (i, col) in row.columns().iter().enumerate() {
        let ty = col.type_();
        let val = if *ty == Type::INT2 || *ty == Type::INT4 || *ty == Type::INT8 {
            row.get::<_, Option<i64>>(i).map(Value::Int).unwrap_or(Value::Unit)
        } else if *ty == Type::FLOAT4 || *ty == Type::FLOAT8 {
            row.get::<_, Option<f64>>(i).map(Value::Float).unwrap_or(Value::Unit)
        } else if *ty == Type::BOOL {
            row.get::<_, Option<bool>>(i).map(Value::Bool).unwrap_or(Value::Unit)
        } else if *ty == Type::BYTEA {
            row.get::<_, Option<Vec<u8>>>(i).map(Value::Bytes).unwrap_or(Value::Unit)
        } else {
            row.get::<_, Option<String>>(i).map(|s| Value::Str(s.into())).unwrap_or(Value::Unit)
        };
        rec.insert(col.name().to_string(), val);
    }
    rec
}

/// Extract a column value from a row record by name, returning `Option[X]`.
fn sql_get_col<F>(args: &[Value], convert: F) -> Result<Value, String>
where
    F: Fn(&Value) -> Option<Value>,
{
    let row = args.first().ok_or("sql.get_*: missing row argument")?;
    let col = match args.get(1) {
        Some(Value::Str(s)) => s.as_str(),
        Some(other) => return Err(format!("sql.get_*: column name must be Str, got {other:?}")),
        None => return Err("sql.get_*: missing column name argument".into()),
    };
    let cell = match row {
        Value::Record { fields: rec, .. } => rec.get(col).cloned(),
        other => return Err(format!("sql.get_*: row must be a Record, got {other:?}")),
    };
    Ok(match cell.and_then(|v| convert(&v)) {
        Some(v) => Value::Variant { name: "Some".into(), args: vec![v] },
        None    => Value::Variant { name: "None".into(), args: vec![] },
    })
}

fn sql_value_ref_to_lex(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null       => Value::Unit,
        ValueRef::Integer(n) => Value::Int(n),
        ValueRef::Real(f)    => Value::Float(f),
        ValueRef::Text(s)    => Value::Str(String::from_utf8_lossy(s).into_owned().into()),
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

// ── std.redis registry (#533) ────────────────────────────────────────
//
// `ConnRedis` is an opaque Int handle into `RedisRegistry`. Each
// `redis.connect` allocates a new handle via `next_redis_handle` and
// stores the open `redis::Connection` plus the original URL (needed to
// open dedicated pub/sub connections for `subscribe`/`psubscribe`).
//
// LRU-bounded at MAX_REDIS_HANDLES to avoid leaks from programs that
// open many short-lived connections without calling `redis.close`.

/// Per-handle state: the live synchronous connection and the URL it
/// was opened from. The URL is kept so `subscribe`/`psubscribe` can
/// open a fresh dedicated connection (Redis forbids non-Pub/Sub
/// commands on a subscribed connection).
struct RedisEntry {
    url: String,
    conn: redis::Connection,
}

struct RedisRegistry {
    entries: indexmap::IndexMap<u64, RedisEntry>,
    cap: usize,
}

impl RedisRegistry {
    fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    fn insert(&mut self, handle: u64, entry: RedisEntry) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, entry);
    }

    fn touch_get_mut(&mut self, handle: u64) -> Option<&mut RedisEntry> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get_mut(&handle)
    }

    /// Return the URL for a handle without touching LRU order. Used by
    /// `subscribe`/`psubscribe` to open a dedicated connection.
    fn get_url(&self, handle: u64) -> Option<String> {
        self.entries.get(&handle).map(|e| e.url.clone())
    }

    fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }
}

fn redis_registry() -> &'static Mutex<RedisRegistry> {
    static REGISTRY: OnceLock<Mutex<RedisRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(RedisRegistry::with_capacity(MAX_REDIS_HANDLES)))
}

const MAX_REDIS_HANDLES: usize = 256;

fn next_redis_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn expect_redis_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected ConnRedis (Int), got {other:?}")),
        None => Err("missing ConnRedis argument".into()),
    }
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

// ── Streaming cursors (#379) ─────────────────────────────────────────
//
// `sql.query_iter[T]` opens a *server-side* cursor and returns an
// `Iter[T]` backed by a producer thread streaming rows through a
// bounded mpsc channel. The bytecode `iter.next` op dispatches on the
// `__IterCursor(handle)` variant tag and effect-calls
// `sql.cursor_next(handle)` to pull one row at a time.
//
// Producer-thread semantics: while the cursor is live, the producer
// holds the underlying SQL connection's `Arc<Mutex<SqlConn>>` lock.
// Other ops on the same Db handle block until the cursor is drained
// or evicted. This matches every server-side cursor protocol
// (sqlite's `sqlite3_step`, Postgres `DECLARE/FETCH`) — neither
// driver supports concurrent statements on a single connection.
//
// Channel capacity: 64 rows. Producer blocks at 64-row backlog,
// keeping resident memory bounded regardless of result-set size.
// Consumer disconnect (Receiver dropped) causes the next send to
// fail, the producer exits, drops the prepared statement, and
// releases the SqlConn lock — so closing a cursor is just "stop
// calling next and let the receiver go out of scope."

const CURSOR_CHANNEL_CAPACITY: usize = 64;
const MAX_CURSOR_HANDLES: usize = 256;

type CursorReceiver = std::sync::mpsc::Receiver<Result<Value, String>>;

pub(crate) struct CursorRegistry {
    /// Each cursor's receiver lives behind its own Mutex so multiple
    /// `sql.cursor_next` calls on the same cursor serialize correctly.
    /// The outer `Arc` lets the global registry lock be released
    /// before blocking on `recv()`.
    entries: indexmap::IndexMap<u64, Arc<Mutex<CursorReceiver>>>,
    cap: usize,
}

impl CursorRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    pub(crate) fn insert(&mut self, handle: u64, rx: CursorReceiver) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, Arc::new(Mutex::new(rx)));
    }

    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<Arc<Mutex<CursorReceiver>>> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle).cloned()
    }

    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }
}

fn cursor_registry() -> &'static Mutex<CursorRegistry> {
    static REGISTRY: OnceLock<Mutex<CursorRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CursorRegistry::with_capacity(MAX_CURSOR_HANDLES)))
}

fn next_cursor_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// SQLite cursor producer: locks the conn, prepares the statement,
/// walks rows, ships each to the consumer through `sender`. Exits on
/// row exhaustion, consumer disconnect, or first error. The lock is
/// released when the thread function returns (statement dropped first
/// to satisfy rusqlite's borrow).
fn sqlite_cursor_producer(
    conn_arc: Arc<Mutex<SqlConn>>,
    stmt_str: String,
    params: Vec<SqlParamValue>,
    sender: std::sync::mpsc::SyncSender<Result<Value, String>>,
) {
    let mut conn_guard = match conn_arc.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let SqlConn::Sqlite(c) = &mut *conn_guard else {
        let _ = sender.send(Err("sqlite_cursor_producer called on non-sqlite conn".into()));
        return;
    };
    let mut stmt = match c.prepare(&stmt_str) {
        Ok(s) => s,
        Err(e) => { let _ = sender.send(Err(format!("prepare: {e}"))); return; }
    };
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let bound = sqlite_params(&params);
    let bind: Vec<&dyn rusqlite::ToSql> =
        bound.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    let mut rows = match stmt.query(rusqlite::params_from_iter(bind.iter())) {
        Ok(r) => r,
        Err(e) => { let _ = sender.send(Err(format!("query: {e}"))); return; }
    };
    loop {
        match rows.next() {
            Ok(None) => break,
            Err(e) => {
                let _ = sender.send(Err(format!("row: {e}")));
                break;
            }
            Ok(Some(row)) => {
                let mut rec = indexmap::IndexMap::new();
                for (i, name) in column_names.iter().enumerate() {
                    let val = match row.get_ref(i) {
                        Ok(vr) => sql_value_ref_to_lex(vr),
                        Err(_) => Value::Unit,
                    };
                    rec.insert(name.clone(), val);
                }
                if sender.send(Ok(Value::record_dynamic(rec))).is_err() {
                    break;
                }
            }
        }
    }
}

/// Postgres cursor producer: opens a transaction + named cursor,
/// fetches rows in batches, ships each one through `sender`. Closes
/// the cursor and commits the transaction on exit.
fn pg_cursor_producer(
    conn_arc: Arc<Mutex<SqlConn>>,
    stmt_str: String,
    params: Vec<SqlParamValue>,
    sender: std::sync::mpsc::SyncSender<Result<Value, String>>,
) {
    let mut conn_guard = match conn_arc.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let SqlConn::Postgres(c) = &mut *conn_guard else {
        let _ = sender.send(Err("pg_cursor_producer called on non-postgres conn".into()));
        return;
    };
    let pg = pg_param_refs(&params);
    let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
        pg.iter().map(|b| b.as_ref()).collect();
    let mut tx = match c.transaction() {
        Ok(t) => t,
        Err(e) => { let _ = sender.send(Err(format!("begin: {e}"))); return; }
    };
    // Use a uniquely-named cursor so concurrent producers on
    // distinct Db handles don't collide on the cursor namespace.
    let cur_name = format!("__lex_cur_{}", next_cursor_handle());
    if let Err(e) = tx.execute(
        &format!("DECLARE \"{cur_name}\" NO SCROLL CURSOR FOR {stmt_str}"),
        &refs,
    ) {
        let _ = sender.send(Err(format!("declare: {e}")));
        return;
    }
    let fetch_sql = format!("FETCH 64 FROM \"{cur_name}\"");
    'outer: loop {
        let batch = match tx.query(&fetch_sql, &[]) {
            Ok(r) => r,
            Err(e) => { let _ = sender.send(Err(format!("fetch: {e}"))); break; }
        };
        if batch.is_empty() {
            break;
        }
        for row in batch.iter() {
            let rec = pg_row_to_lex_record(row);
            if sender.send(Ok(Value::record_dynamic(rec))).is_err() {
                break 'outer;
            }
        }
    }
    let _ = tx.execute(&format!("CLOSE \"{cur_name}\""), &[]);
    let _ = tx.commit();
}

/// Driver-neutral SQL parameter value shared between SQLite and Postgres paths.
#[derive(Debug, Clone)]
enum SqlParamValue {
    Text(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
    Null,
}

/// Abstraction over a SQLite connection or a Postgres client.
pub(crate) enum SqlConn {
    Sqlite(rusqlite::Connection),
    Postgres(postgres::Client),
}

type SharedConn = Arc<Mutex<SqlConn>>;

pub(crate) struct SqlRegistry {
    entries: indexmap::IndexMap<u64, SharedConn>,
    cap: usize,
}

impl SqlRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    pub(crate) fn insert(&mut self, handle: u64, conn: SqlConn) {
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
    use super::{SqlConn, SqlRegistry};

    fn fresh() -> SqlConn {
        SqlConn::Sqlite(rusqlite::Connection::open_in_memory().expect("open in-memory sqlite"))
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

/// #463 slab-direct wire-up — locally-runnable coverage for
/// `unpack_response`'s arena path. The lex-runtime integration tests
/// (`tests/std_http.rs` etc.) overflow the dev-container disk per
/// `arena-plumbing.md`, so CI is the only place they run end-to-end;
/// these focused tests give us a local regression gate on the
/// boundary code itself.
#[cfg(test)]
mod unpack_response_tests {
    use super::*;
    use std::sync::Arc;
    use indexmap::IndexMap;
    use lex_bytecode::{Const, Op, Program, Value};
    use lex_bytecode::program::{Function, ZERO_BODY_HASH};
    use lex_bytecode::vm::Vm;

    /// Build a single-fn `Program` whose body produces an
    /// `AllocArenaRecord`-backed `Response { status, body }`. The
    /// constants table holds the field names, the body variant name,
    /// the response text, and the status code.
    fn build_arena_response_program() -> Arc<Program> {
        let constants = vec![
            Const::FieldName("status".into()), // 0
            Const::FieldName("body".into()),   // 1
            Const::Int(200),                   // 2
            Const::VariantName("BodyStr".into()), // 3
            Const::Str("hello".into()),        // 4
        ];
        let mut function_names = IndexMap::new();
        function_names.insert("handler".to_string(), 0);
        Arc::new(Program {
            constants,
            functions: vec![Function {
                name: "handler".into(),
                arity: 0,
                locals_count: 0,
                code: vec![
                    Op::PushConst(2),                                       // 200
                    Op::PushConst(4),                                       // "hello"
                    Op::MakeVariant { name_idx: 3, arity: 1 },              // BodyStr("hello")
                    Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },  // { status, body }
                    Op::Return,
                ],
                effects: vec![],
                body_hash: ZERO_BODY_HASH,
                refinements: vec![],
                field_ic_sites: 0,
            }],
            function_names,
            module_aliases: IndexMap::new(),
            entry: Some(0),
            record_shapes: vec![vec![0, 1]], // {status, body}
        })
    }

    /// The happy path: arena handle goes in, the unpacked tuple comes
    /// out, no `materialize_arena_handles` walk in between. The
    /// boundary call site no longer holds a heap `Value::Record` —
    /// `unpack_response` reads straight out of the slab via
    /// `Vm::get_record_field`.
    #[test]
    fn unpack_response_reads_arena_record_via_slab() {
        let p = build_arena_response_program();
        let mut vm = Vm::new(&p);
        let scope = vm.enter_request_scope();

        let resp = vm.invoke(p.function_names["handler"], vec![]).unwrap();
        // Test precondition — without this the slab-direct path isn't
        // being exercised at all.
        assert!(matches!(resp, Value::ArenaRecord { .. }),
            "expected ArenaRecord (slab path), got {resp:?}");

        let (status, body, headers) = unpack_response(&mut vm, &resp);
        vm.exit_request_scope(scope);

        assert_eq!(status, 200);
        assert!(headers.is_empty());
        match body {
            ResponseBodyOut::Str(s) => assert_eq!(s, "hello"),
            _ => panic!("expected BodyStr"),
        }
    }

    /// Heap path uniformity: a handler that returns a plain
    /// `Value::Record` (no arena scope, or a non-arena-lowered site)
    /// produces the same tuple. The same `unpack_response` is the
    /// single chokepoint.
    #[test]
    fn unpack_response_reads_heap_record() {
        let p = build_arena_response_program();
        let mut vm = Vm::new(&p);

        // No scope — `AllocArenaRecord` falls back to heap `MakeRecord`.
        let resp = vm.invoke(p.function_names["handler"], vec![]).unwrap();
        assert!(matches!(resp, Value::Record { .. }),
            "expected heap Record (fallback path), got {resp:?}");

        let (status, body, headers) = unpack_response(&mut vm, &resp);
        assert_eq!(status, 200);
        assert!(headers.is_empty());
        match body {
            ResponseBodyOut::Str(s) => assert_eq!(s, "hello"),
            _ => panic!("expected BodyStr"),
        }
    }

    /// Defaults: handler returns a non-record. The error path produces
    /// a 500 with a diagnostic. Unchanged from pre-wire-up behavior.
    #[test]
    fn unpack_response_falls_back_to_500_on_non_record() {
        let p = build_arena_response_program();
        let mut vm = Vm::new(&p);
        let v = Value::Int(7);
        let (status, _body, _headers) = unpack_response(&mut vm, &v);
        assert_eq!(status, 500);
    }
}
