//! Hyper + Tokio HTTP server backend for `net.serve_fn` / `net.serve`
//! (#388). Replaces the previous blocking `tiny_http` backend for
//! plaintext connections.
//!
//! The Lex-facing surface is unchanged — handlers still receive a
//! `Request` record, return a `Response` record, run synchronously per
//! request. What changes is the I/O layer beneath:
//!
//! - **Multi-thread Tokio runtime** spun up at server start. The
//!   runtime drives the accept loop and per-connection HTTP/1.1
//!   framing.
//! - **`hyper::server::conn::http1::Builder`** handles request
//!   parsing, header framing, response serialisation, and keep-alive.
//! - **Per-request `spawn_blocking`** moves the synchronous Lex VM
//!   call onto Tokio's blocking thread pool so the async accept loop
//!   isn't starved while a handler runs.
//! - **`http-body-util`** streams the `BodyStream` / `BodyBytes`
//!   chunk lists out via `StreamBody` instead of the old `Read`-on-
//!   chunked-transfer adapter. `BodyStr` uses `Full` (eager,
//!   `Content-Length` set).
//!
//! Effect-policy gating is unchanged — it runs at dispatch time
//! before this module is ever entered, so the only effect kind that
//! matters here is `[net]` itself.
//!
//! TLS (`net.serve_tls`) is still served by `tiny_http` for now; the
//! follow-up will migrate it to `tokio-rustls` + `hyper`.

use bytes::Bytes;
use futures_util::stream;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use lex_bytecode::vm::Vm;
use lex_bytecode::{MapKey, Program, Value};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::policy::Policy;

/// One outbound response body, decoded from a Lex `ResponseBody`
/// variant (`BodyStr` / `BodyStream(Iter[Str])` / `BodyBytes(Iter[List[Int]])`)
/// into a backend-neutral shape.
pub enum DecodedBody {
    Str(String),
    /// One Vec<u8> per Lex iter item — preserves chunk boundaries on
    /// the wire as separate HTTP chunks when fed through StreamBody.
    Chunks(Vec<Vec<u8>>),
}

/// Decoded `Response`-record produced from a Lex handler's return
/// value. Backend-neutral so the same shape feeds both hyper and the
/// remaining tiny_http TLS path.
pub struct DecodedResponse {
    pub status: u16,
    pub body: DecodedBody,
    pub headers: Vec<(String, String)>,
}

// ── HTTP-server entry points ────────────────────────────────────────────────

/// `net.serve_fn(port, closure)` — closure-based plaintext HTTP server.
/// Blocks for the lifetime of the server (returns `Value::Unit` only on
/// shutdown, which the current API doesn't expose).
pub fn serve_http_fn(
    port: u16,
    closure: Value,
    program: Arc<Program>,
    policy: Policy,
) -> Result<Value, String> {
    let rt = tokio_runtime("net.serve_fn")?;
    rt.block_on(async move {
        run_server(port, Dispatcher::Closure(closure), program, policy, "net.serve_fn").await
    })
}

/// `net.serve(port, handler_name)` — by-name plaintext HTTP server.
/// Same multi-thread Tokio runtime as `serve_http_fn`; the only
/// difference is the per-request dispatcher.
pub fn serve_http_by_name(
    port: u16,
    handler_name: String,
    program: Arc<Program>,
    policy: Policy,
) -> Result<Value, String> {
    let rt = tokio_runtime("net.serve")?;
    rt.block_on(async move {
        run_server(port, Dispatcher::Named(handler_name), program, policy, "net.serve").await
    })
}

fn tokio_runtime(op: &str) -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("{op} tokio runtime: {e}"))
}

/// Distinguishes the by-name and closure-based handler dispatch
/// inside the per-connection service function.
#[derive(Clone)]
enum Dispatcher {
    Named(String),
    Closure(Value),
}

async fn run_server(
    port: u16,
    dispatcher: Dispatcher,
    program: Arc<Program>,
    policy: Policy,
    op_label: &str,
) -> Result<Value, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("{op_label} bind {port}: {e}"))?;
    eprintln!("{op_label}: listening on http://127.0.0.1:{port}");

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("{op_label} accept: {e}");
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let program = Arc::clone(&program);
        let policy = policy.clone();
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let program = Arc::clone(&program);
                let policy = policy.clone();
                let dispatcher = dispatcher.clone();
                async move { handle_request(req, dispatcher, program, policy).await }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, svc)
                .await
            {
                eprintln!("hyper connection error: {e}");
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    dispatcher: Dispatcher,
    program: Arc<Program>,
    policy: Policy,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let lex_req = match decode_request(req).await {
        Ok(v) => v,
        Err(msg) => return Ok(error_response(StatusCode::BAD_REQUEST, msg)),
    };

    // Move the synchronous VM call onto Tokio's blocking pool so the
    // async accept loop isn't starved. The VM internally is purely
    // synchronous — any [io] / [time] / etc. effects the closure
    // invokes block this worker, not the runtime.
    let program_for_blocking = Arc::clone(&program);
    let vm_result = tokio::task::spawn_blocking(move || {
        let handler = crate::handler::DefaultHandler::new(policy.clone())
            .with_program(Arc::clone(&program_for_blocking));
        let mut vm = Vm::with_handler(&program_for_blocking, Box::new(handler));
        match dispatcher {
            Dispatcher::Named(name) => vm.call(&name, vec![lex_req]),
            Dispatcher::Closure(closure) => vm.invoke_closure_value(closure, vec![lex_req]),
        }
    })
    .await;

    let resp_value = match vm_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("internal error: {e}"),
            ));
        }
        Err(e) => {
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("handler join error: {e}"),
            ));
        }
    };

    Ok(build_response(decode_response(&resp_value)))
}

// ── Request decoding ────────────────────────────────────────────────────────

/// Translate a hyper `Request<Incoming>` into the Lex `Request`
/// record shape the handlers expect.
async fn decode_request(req: Request<Incoming>) -> Result<Value, String> {
    let method = method_str(req.method());
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();

    let mut headers_map = BTreeMap::new();
    for (name, val) in req.headers().iter() {
        // Header values that aren't valid UTF-8 are skipped to match
        // tiny_http's `as_str()` behaviour — Lex `Str` can't hold raw
        // bytes anyway. Callers needing binary headers have already
        // been an open question; punt for now.
        if let Ok(s) = val.to_str() {
            headers_map.insert(
                MapKey::Str(name.as_str().to_ascii_lowercase()),
                Value::Str(s.to_string()),
            );
        }
    }

    // Read the entire body into a String. Matches tiny_http's
    // `as_reader().read_to_string(&mut body)` contract — handlers
    // expect a Str. Binary uploads are a follow-up (Bytes-typed
    // body field).
    let body_bytes = req
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("request body read: {e}"))?
        .to_bytes();
    let body = String::from_utf8_lossy(&body_bytes).into_owned();

    let mut rec = indexmap::IndexMap::new();
    rec.insert("method".into(), Value::Str(method));
    rec.insert("path".into(), Value::Str(path));
    rec.insert("query".into(), Value::Str(query));
    rec.insert("body".into(), Value::Str(body));
    rec.insert("headers".into(), Value::Map(headers_map));
    Ok(Value::Record(rec))
}

fn method_str(method: &Method) -> String {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::CONNECT => "CONNECT",
        Method::TRACE => "TRACE",
        _ => method.as_str(),
    }
    .to_string()
}

// ── Response decoding (Lex → DecodedResponse) ──────────────────────────────

/// Pull the Lex handler's `Response`-shaped return value apart into
/// the backend-neutral `DecodedResponse`. Same shape contract the
/// tiny_http path uses — both reasonable structural and nominal
/// `Response` types work, and a missing `status` defaults to 200.
pub fn decode_response(v: &Value) -> DecodedResponse {
    if let Value::Record(rec) = v {
        let status = rec
            .get("status")
            .and_then(|s| match s {
                Value::Int(n) => Some(*n as u16),
                _ => None,
            })
            .unwrap_or(200);

        let body = match rec.get("body") {
            Some(Value::Variant { name, args }) => match (name.as_str(), args.as_slice()) {
                ("BodyStr", [Value::Str(s)]) => DecodedBody::Str(s.clone()),
                ("BodyStream", [iter_v]) => DecodedBody::Chunks(drain_iter_str(iter_v)),
                ("BodyBytes", [iter_v]) => DecodedBody::Chunks(drain_iter_bytes(iter_v)),
                _ => DecodedBody::Str(String::new()),
            },
            // Pre-#375 escape hatch: handlers returning a structural
            // record with a plain `Str` body.
            Some(Value::Str(s)) => DecodedBody::Str(s.clone()),
            _ => DecodedBody::Str(String::new()),
        };

        let headers = match rec.get("headers") {
            Some(Value::Map(hmap)) => hmap
                .iter()
                .filter_map(|(k, v)| match (k, v) {
                    (MapKey::Str(name), Value::Str(val)) => {
                        Some((name.clone(), val.clone()))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };

        return DecodedResponse { status, body, headers };
    }
    DecodedResponse {
        status: 500,
        body: DecodedBody::Str(format!("handler returned non-record: {v:?}")),
        headers: Vec::new(),
    }
}

/// Drain a Lex `Iter[Str]` into a chunk-of-bytes list.
///
/// Eager iters (`__IterEager`) are walked positionally. Lazy
/// (`__IterLazy`) and cursor-backed (`__IterCursor`) variants are
/// not handled here yet — same v1 limitation the tiny_http path
/// had. The follow-up that makes lazy iters drive each `read()`
/// will let us hand `StreamBody` a real async stream that pulls
/// from `iter.next` on demand.
fn drain_iter_str(v: &Value) -> Vec<Vec<u8>> {
    match v {
        Value::Variant { name, args } if name == "__IterEager" && args.len() == 2 => {
            if let (Value::List(items), Value::Int(idx)) = (&args[0], &args[1]) {
                items
                    .iter()
                    .skip(*idx as usize)
                    .filter_map(|item| match item {
                        Value::Str(s) => Some(s.as_bytes().to_vec()),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn drain_iter_bytes(v: &Value) -> Vec<Vec<u8>> {
    match v {
        Value::Variant { name, args } if name == "__IterEager" && args.len() == 2 => {
            if let (Value::List(items), Value::Int(idx)) = (&args[0], &args[1]) {
                items
                    .iter()
                    .skip(*idx as usize)
                    .filter_map(|item| match item {
                        Value::List(ints) => Some(
                            ints.iter()
                                .filter_map(|i| match i {
                                    Value::Int(n) => Some((*n & 0xff) as u8),
                                    _ => None,
                                })
                                .collect::<Vec<u8>>(),
                        ),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

// ── DecodedResponse → hyper Response ───────────────────────────────────────

fn build_response(d: DecodedResponse) -> Response<BoxBody<Bytes, Infallible>> {
    let status =
        StatusCode::from_u16(d.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);

    let mut had_content_length = false;
    for (name, val) in &d.headers {
        if name.eq_ignore_ascii_case("content-length") {
            had_content_length = true;
        }
        builder = builder.header(name, val);
    }

    let body: BoxBody<Bytes, Infallible> = match d.body {
        DecodedBody::Str(s) => {
            // hyper sets Content-Length automatically from the
            // `Full` body length; suppress duplicates if the
            // handler also set one explicitly.
            let _ = had_content_length;
            Full::new(Bytes::from(s)).boxed()
        }
        DecodedBody::Chunks(chunks) => {
            // StreamBody → hyper emits Transfer-Encoding: chunked
            // (one HTTP chunk per Lex iter item, since each
            // `Frame::data` lands as its own write under http1
            // framing). No Content-Length on the wire.
            let frames = chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Frame::data(Bytes::from(c))));
            StreamBody::new(stream::iter(frames)).boxed()
        }
    };

    builder.body(body).unwrap_or_else(|e| {
        // Should be unreachable — Response::builder() only fails on
        // invalid header names, which we got from the user's Lex
        // Map keys. Bail to a 500 if it does.
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("response build: {e}"),
        )
    })
}

fn error_response(status: StatusCode, msg: String) -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(msg)).boxed())
        .unwrap()
}
