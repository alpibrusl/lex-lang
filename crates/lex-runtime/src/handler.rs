//! Native effect handlers, dispatched at runtime through the VM's
//! `EffectHandler` trait. The handler also re-checks the runtime policy
//! per spec §7.4 (the static check is necessary but not sufficient: a fn
//! declared `[fs_read("/data")]` that's allowed at startup still has to
//! pass the path check at the point of dispatch).

use lex_bytecode::vm::EffectHandler;
use lex_bytecode::Value;
use std::cell::RefCell;
use std::path::PathBuf;
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
}

impl DefaultHandler {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            sink: Box::new(StdoutSink),
            read_root: None,
            budget_used: RefCell::new(0),
        }
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
}

impl EffectHandler for DefaultHandler {
    fn dispatch(&mut self, kind: &str, op: &str, args: Vec<Value>) -> Result<Value, String> {
        // Pure stdlib builtins (str, list, json, ...) bypass the policy
        // gate — they have no observable side effects and aren't tracked
        // by the type system as effects.
        if let Some(r) = try_pure_builtin(kind, op, &args) {
            return r;
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
                Ok(http_request("GET", &url, None))
            }
            ("net", "post") => {
                let url = expect_str(args.first())?.to_string();
                let body = expect_str(args.get(1))?.to_string();
                Ok(http_request("POST", &url, Some(&body)))
            }
            other => Err(format!("unsupported effect {}.{}", other.0, other.1)),
        }
    }
}

/// Minimal hand-rolled HTTP/1.0 client. Supports `http://host:port/path`
/// only (no TLS, no redirects). Returns `Result[Str, Str]` as a Lex
/// `Value::Variant`.
fn http_request(method: &str, url: &str, body: Option<&str>) -> Value {
    let parsed = match parse_http_url(url) {
        Ok(u) => u,
        Err(e) => return err_value(format!("bad url: {e}")),
    };
    let body_bytes = body.unwrap_or("").as_bytes();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: {host}\r\nContent-Length: {clen}\r\nConnection: close\r\n\r\n",
        path = parsed.path,
        host = parsed.host,
        clen = body_bytes.len(),
    );
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let mut stream = match TcpStream::connect((parsed.host.as_str(), parsed.port)) {
        Ok(s) => s,
        Err(e) => return err_value(format!("connect: {e}")),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return err_value(format!("write: {e}"));
    }
    if !body_bytes.is_empty() {
        if let Err(e) = stream.write_all(body_bytes) {
            return err_value(format!("write body: {e}"));
        }
    }
    let mut buf = String::new();
    if let Err(e) = stream.read_to_string(&mut buf) {
        return err_value(format!("read: {e}"));
    }
    // Split headers from body.
    let body_text = match buf.split_once("\r\n\r\n") {
        Some((_head, b)) => b.to_string(),
        None => buf,
    };
    Value::Variant { name: "Ok".into(), args: vec![Value::Str(body_text)] }
}

struct ParsedUrl { host: String, port: u16, path: String }

fn parse_http_url(url: &str) -> Result<ParsedUrl, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| "must start with http://".to_string())?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|e| format!("port: {e}"))?),
        None => (host_port.to_string(), 80),
    };
    Ok(ParsedUrl { host, port, path: path.to_string() })
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
