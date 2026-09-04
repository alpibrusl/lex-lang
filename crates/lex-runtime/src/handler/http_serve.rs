//! HTTP servers behind `net.serve*`: tiny_http / hyper / TLS / QUIC entry points, route matching, `ServeOpts`, and the request / response conversion between Lex records and the wire.

use super::*;

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

pub(super) fn serve_http(
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
pub(super) fn serve_http_plain(
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
pub(super) fn serve_http_tls_legacy(
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

pub(super) fn handle_request_tls(
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
pub(super) fn serve_http_fn(
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
pub(super) fn compile_path_pattern(pat: &str) -> Result<Vec<RouteSeg>, String> {
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
pub(super) fn match_path_pattern(
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
pub(super) fn decode_routes_arg(
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
pub(super) fn serve_http_routed(
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
pub(super) fn env_inline_vm() -> bool {
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
    pub(super) fn from_env() -> Self {
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
    pub(super) fn lex_defaults() -> Self {
        Self {
            http2: false,
            inline_vm: false,
            host: "0.0.0.0".to_string(),
        }
    }

    /// Convert to a Lex `Value::Record` for return from `default_opts()`.
    pub(super) fn to_value(&self) -> Value {
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
pub(super) fn decode_serve_opts(v: &Value) -> Result<ServeOpts, String> {
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

pub(super) fn make_tls_config_value(cert_pem: Vec<u8>, key_pem: Vec<u8>) -> Value {
    let mut rec = indexmap::IndexMap::new();
    rec.insert("cert".into(), Value::Bytes(cert_pem));
    rec.insert("key".into(),  Value::Bytes(key_pem));
    Value::record_dynamic(rec)
}

#[cfg(feature = "quic")]
pub(super) fn decode_tls_config(v: &Value) -> Result<crate::quic::QuicTls, String> {
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

pub(super) fn dispatch_tls_from_pem_files(
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
pub(super) fn dispatch_tls_self_signed(args: Vec<Value>) -> Result<Value, String> {
    let hostname = expect_str(args.first())?.to_string();
    match crate::quic::self_signed_pem(&hostname) {
        Ok((cert, key)) => Ok(ok(make_tls_config_value(cert, key))),
        Err(e) => Ok(err(Value::Str(format!("tls.self_signed: {e}").into()))),
    }
}

#[cfg(not(feature = "quic"))]
pub(super) fn dispatch_tls_self_signed(_args: Vec<Value>) -> Result<Value, String> {
    Ok(err(Value::Str(
        "tls.self_signed: lex-runtime was compiled without the `quic` feature (needed for rcgen)".into(),
    )))
}

impl DefaultHandler {
    #[cfg(feature = "quic")]
    pub(super) fn dispatch_serve_quic_named(&self, args: Vec<Value>) -> Result<Value, String> {
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
    pub(super) fn dispatch_serve_quic_fn(&self, args: Vec<Value>) -> Result<Value, String> {
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
    pub(super) fn dispatch_serve_quic_routed(&self, args: Vec<Value>) -> Result<Value, String> {
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
    pub(super) fn dispatch_serve_quic_named(&self, _args: Vec<Value>) -> Result<Value, String> {
        Err("net.serve_quic: lex-runtime was compiled without the `quic` feature (needed for quinn + h3)".into())
    }
    #[cfg(not(feature = "quic"))]
    pub(super) fn dispatch_serve_quic_fn(&self, _args: Vec<Value>) -> Result<Value, String> {
        Err("net.serve_quic_fn: lex-runtime was compiled without the `quic` feature (needed for quinn + h3)".into())
    }
    #[cfg(not(feature = "quic"))]
    pub(super) fn dispatch_serve_quic_routed(&self, _args: Vec<Value>) -> Result<Value, String> {
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
pub(super) fn env_http2() -> bool {
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
pub(super) fn build_request_value_tiny(req: &mut tiny_http::Request) -> Value {
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

pub(super) type HyperRespBody =
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
pub(super) fn build_hyper_response(
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

pub(super) fn error_response(status: u16, msg: &str) -> hyper::Response<HyperRespBody> {
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
pub(super) struct HyperChunkedBody {
    pub(super) chunks: std::collections::VecDeque<Vec<u8>>,
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
pub(super) fn respond_with_body_tls(
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
pub(super) fn drain_iter_str(v: &Value) -> Vec<Vec<u8>> {
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
pub(super) fn drain_iter_bytes(v: &Value) -> Vec<Vec<u8>> {
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
pub(super) fn materialize_lazy_iter(vm: &mut Vm, v: Value) -> Value {
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
pub(super) struct ChunkReader {
    pub(super) chunks: std::collections::VecDeque<Vec<u8>>,
    pub(super) cursor: usize,
}

impl ChunkReader {
    pub(super) fn new(chunks: Vec<Vec<u8>>) -> Self {
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
