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

mod approval;
mod dispatch;
mod fs;
mod http_client;
mod http_serve;
mod kv;
mod llm;
mod logging;
mod proc;
mod redis_store;
mod sql;
mod udp;

pub use approval::{ApprovalSink, NullApprovalSink, StdinApprovalSink};
pub use http_serve::TlsConfig;
#[cfg(feature = "quic")]
pub(crate) use http_serve::{
    build_request_value_parts, dispatch_route, stamp_path_params, unpack_response, ResponseBodyOut,
    RouteSeg, ServeOpts, UnpackedResponse,
};
use http_client::*;
use http_serve::*;
use kv::*;
use llm::*;
use redis_store::*;
use sql::*;
use udp::*;

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
    /// Host boundary for `approval.request`. Defaults to
    /// `NullApprovalSink` (always refuses) so a handler must opt in
    /// via `with_approval_sink` before `[approval]` calls can succeed.
    pub approval_sink: Box<dyn ApprovalSink>,
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
            approval_sink: Box::new(NullApprovalSink),
        }
    }

    pub fn with_approval_sink(mut self, sink: Box<dyn ApprovalSink>) -> Self {
        self.approval_sink = sink; self
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

// Root of the process content store for std.vcs (#5). Matches the store-using
// CLI commands (branch/op): $LEX_STORE_ROOT override, else ~/.lex/store.
fn vcs_store_root() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("LEX_STORE_ROOT") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    home.join(".lex/store")
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

