//! Native effect handlers, dispatched at runtime through the VM's
//! `EffectHandler` trait. The handler also re-checks the runtime policy
//! per spec §7.4 (the static check is necessary but not sufficient: a fn
//! declared `[fs_read("/data")]` that's allowed at startup still has to
//! pass the path check at the point of dispatch).

use lex_bytecode::vm::{EffectHandler, Vm};
use lex_bytecode::{Program, Value};
use std::cell::RefCell;
use std::collections::HashMap;
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
    /// Captured budget consumption (post-static-check, observability only).
    pub budget_used: RefCell<u64>,
    /// Shared reference to the program, needed by `net.serve` so the
    /// handler can spin up fresh VMs to dispatch incoming requests.
    /// `None` if the handler was constructed without a program.
    pub program: Option<Arc<Program>>,
    /// Chat registry; populated by `net.serve_ws`'s per-message
    /// dispatch so `chat.broadcast` / `chat.send` work from inside
    /// a handler invocation.
    pub chat_registry: Option<Arc<crate::ws::ChatRegistry>>,
}

impl DefaultHandler {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            sink: Box::new(StdoutSink),
            read_root: None,
            budget_used: RefCell::new(0),
            program: None,
            chat_registry: None,
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
            ("rand", "int_in") => {
                // Deterministic stub: midpoint of [lo, hi].
                let lo = expect_int(args.first())?;
                let hi = expect_int(args.get(1))?;
                Ok(Value::Int((lo + hi) / 2))
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
                kv_registry().lock().unwrap().remove(&h);
                Ok(Value::Unit)
            }
            ("kv", "get") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let reg = kv_registry().lock().unwrap();
                let db = reg.get(&h).ok_or_else(|| "kv.get: closed or unknown Kv handle".to_string())?;
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
                let reg = kv_registry().lock().unwrap();
                let db = reg.get(&h).ok_or_else(|| "kv.put: closed or unknown Kv handle".to_string())?;
                match db.insert(key.as_bytes(), val) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("kv.put: {e}")))),
                }
            }
            ("kv", "delete") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let reg = kv_registry().lock().unwrap();
                let db = reg.get(&h).ok_or_else(|| "kv.delete: closed or unknown Kv handle".to_string())?;
                match db.remove(key.as_bytes()) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("kv.delete: {e}")))),
                }
            }
            ("kv", "contains") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let reg = kv_registry().lock().unwrap();
                let db = reg.get(&h).ok_or_else(|| "kv.contains: closed or unknown Kv handle".to_string())?;
                match db.contains_key(key.as_bytes()) {
                    Ok(present) => Ok(Value::Bool(present)),
                    Err(e) => Err(format!("kv.contains: {e}")),
                }
            }
            ("kv", "list_prefix") => {
                let h = expect_kv_handle(args.first())?;
                let prefix = expect_str(args.get(1))?;
                let reg = kv_registry().lock().unwrap();
                let db = reg.get(&h).ok_or_else(|| "kv.list_prefix: closed or unknown Kv handle".to_string())?;
                let mut keys: Vec<Value> = Vec::new();
                for kv in db.scan_prefix(prefix.as_bytes()) {
                    let (k, _) = kv.map_err(|e| format!("kv.list_prefix: {e}"))?;
                    let s = String::from_utf8_lossy(&k).to_string();
                    keys.push(Value::Str(s));
                }
                Ok(Value::List(keys))
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

/// Process-wide registry of open `Kv` handles. Each `kv.open` allocates
/// a new u64 handle via [`next_kv_handle`] and stores the `sled::Db`
/// here; subsequent ops fetch by handle. `kv.close` removes the entry.
fn kv_registry() -> &'static Mutex<HashMap<u64, sled::Db>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, sled::Db>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_kv_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}
