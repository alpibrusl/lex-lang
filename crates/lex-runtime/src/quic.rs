//! HTTP/3 server impl behind the `quic` feature flag (#496).
//!
//! Parallels `serve_http_plain` / `serve_http_fn` / `serve_http_routed`
//! in `handler.rs` but on a different transport: QUIC over UDP via
//! `quinn`, with `h3` as the HTTP/3 wire-protocol layer. TLS is
//! mandatory in HTTP/3 — there's no plaintext equivalent. Callers
//! pass a `TlsConfig` (built via `tls.from_pem_files` or
//! `tls.self_signed`) which we feed into a rustls server config
//! tagged with the `h3` ALPN.
//!
//! Design decisions (from issue #496):
//! - Effect row stays `[net]` — same gate as `serve` / `serve_fn`.
//! - 0-RTT is OFF by default — rustls's default is "disabled" so we
//!   don't need to do anything special. Opting in would require
//!   `ServerConfig::max_early_data_size` to be set non-zero AND
//!   per-route replay protection at the handler level (out of scope
//!   for v1).
//! - No cert reload in v1 — rotation requires restarting the
//!   listener.
//!
//! Dispatcher shape mirrors the TCP path: one of three modes — a
//! named handler, a closure, or a routed table — picked at the
//! dispatch arm in `handler.rs` and threaded through here.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use lex_bytecode::value::Value;
use lex_bytecode::vm::Vm;
use lex_bytecode::Program;

use crate::handler::{
    self, dispatch_route, stamp_path_params, RouteSeg, ServeOpts,
};
use crate::policy::Policy;
use crate::DefaultHandler;

/// PEM-encoded certificate chain + private key as raw bytes. Built
/// inside `handler.rs` when decoding the opaque Lex `TlsConfig` value
/// passed to `net.serve_quic`. Held by reference in `serve_*` so the
/// underlying bytes don't get cloned per-request.
pub(crate) struct QuicTls {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

pub(crate) enum Dispatcher {
    Named(String),
    Closure(Value),
    Routed {
        routes: Vec<(String, Vec<RouteSeg>, Value)>,
        fallback: Value,
    },
}

pub(crate) fn serve_http3_named(
    port: u16,
    handler_name: String,
    tls: QuicTls,
    program: Arc<Program>,
    policy: Policy,
    opts: ServeOpts,
) -> Result<Value, String> {
    serve_http3(port, tls, program, policy, opts, Dispatcher::Named(handler_name))
}

pub(crate) fn serve_http3_fn(
    port: u16,
    closure: Value,
    tls: QuicTls,
    program: Arc<Program>,
    policy: Policy,
    opts: ServeOpts,
) -> Result<Value, String> {
    serve_http3(port, tls, program, policy, opts, Dispatcher::Closure(closure))
}

pub(crate) fn serve_http3_routed(
    port: u16,
    routes: Vec<(String, Vec<RouteSeg>, Value)>,
    fallback: Value,
    tls: QuicTls,
    program: Arc<Program>,
    policy: Policy,
    opts: ServeOpts,
) -> Result<Value, String> {
    serve_http3(
        port,
        tls,
        program,
        policy,
        opts,
        Dispatcher::Routed { routes, fallback },
    )
}

fn build_server_config(tls: &QuicTls) -> Result<quinn::ServerConfig, String> {
    // Install ring as the rustls default provider on first call. The
    // process-global registration is idempotent — ignore "already set"
    // errors so a second serve_quic call in the same process works.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut cert_pem = tls.cert_pem.as_slice();
    let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("net.serve_quic: parse cert PEM: {e}"))?;
    if cert_chain.is_empty() {
        return Err("net.serve_quic: no certificates found in cert PEM".into());
    }

    let mut key_pem = tls.key_pem.as_slice();
    let key_der = rustls_pemfile::private_key(&mut key_pem)
        .map_err(|e| format!("net.serve_quic: parse key PEM: {e}"))?
        .ok_or_else(|| "net.serve_quic: no private key found in key PEM".to_string())?;

    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .map_err(|e| format!("net.serve_quic: rustls server config: {e}"))?;
    // HTTP/3 negotiates via the `h3` ALPN token — clients that don't
    // advertise it (HTTP/1.1, HTTP/2 over TCP) wouldn't reach this
    // listener anyway since QUIC is UDP, but rustls still requires
    // ALPN to be set when serving h3.
    crypto.alpn_protocols = vec![b"h3".to_vec()];

    let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .map_err(|e| format!("net.serve_quic: quic server config: {e}"))?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(qsc)))
}

fn serve_http3(
    port: u16,
    tls: QuicTls,
    program: Arc<Program>,
    policy: Policy,
    opts: ServeOpts,
    dispatcher: Dispatcher,
) -> Result<Value, String> {
    let server_config = build_server_config(&tls)?;
    let host = opts.host.clone();
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("net.serve_quic: parse {host}:{port}: {e}"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("net.serve_quic: tokio runtime: {e}"))?;

    let dispatcher = Arc::new(dispatcher);

    rt.block_on(async move {
        let endpoint = quinn::Endpoint::server(server_config, addr)
            .map_err(|e| format!("net.serve_quic: bind {addr}: {e}"))?;
        eprintln!("net.serve_quic: listening on https://{addr} (HTTP/3)");

        while let Some(incoming) = endpoint.accept().await {
            let program = Arc::clone(&program);
            let policy = policy.clone();
            let dispatcher = Arc::clone(&dispatcher);
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("net.serve_quic: handshake: {e}");
                        return;
                    }
                };
                if let Err(e) = handle_quic_conn(conn, program, policy, dispatcher).await {
                    eprintln!("net.serve_quic: connection: {e}");
                }
            });
        }
        Ok(Value::Unit)
    })
}

async fn handle_quic_conn(
    conn: quinn::Connection,
    program: Arc<Program>,
    policy: Policy,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), String> {
    let h3_conn = h3_quinn::Connection::new(conn);
    let mut h3 = h3::server::Connection::new(h3_conn)
        .await
        .map_err(|e| format!("h3 connection: {e}"))?;

    loop {
        match h3.accept().await {
            Ok(Some(resolver)) => {
                let program = Arc::clone(&program);
                let policy = policy.clone();
                let dispatcher = Arc::clone(&dispatcher);
                tokio::spawn(async move {
                    let (req, stream) = match resolver.resolve_request().await {
                        Ok(rs) => rs,
                        Err(e) => {
                            eprintln!("net.serve_quic: resolve_request: {e}");
                            return;
                        }
                    };
                    if let Err(e) =
                        handle_h3_request(req, stream, program, policy, dispatcher).await
                    {
                        eprintln!("net.serve_quic: request: {e}");
                    }
                });
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("net.serve_quic: h3 accept: {e}");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_h3_request<S>(
    req: hyper::http::Request<()>,
    mut stream: h3::server::RequestStream<S, Bytes>,
    program: Arc<Program>,
    policy: Policy,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), String>
where
    S: h3::quic::BidiStream<Bytes> + Send + 'static,
    <S as h3::quic::BidiStream<Bytes>>::SendStream: Send,
    <S as h3::quic::BidiStream<Bytes>>::RecvStream: Send,
{
    let (parts, _) = req.into_parts();

    // Drain the request body. h3's `recv_data()` returns successive
    // chunks until the peer finishes; collect into a single Bytes
    // buffer to feed Lex's body :: Str field.
    let mut body = Vec::new();
    while let Some(mut chunk) = stream
        .recv_data()
        .await
        .map_err(|e| format!("h3 recv body: {e}"))?
    {
        use bytes::Buf as _;
        let remaining = chunk.remaining();
        body.extend_from_slice(chunk.copy_to_bytes(remaining).as_ref());
    }
    let body = Bytes::from(body);

    // Build the Lex request record from the same hyper parts shape
    // the HTTP/1.1+2 path uses — `parts.method`, `parts.uri`,
    // `parts.headers` are all the `http` crate types, common to
    // hyper and h3.
    let lex_req = handler::build_request_value_parts(&parts, &body);
    let method = parts.method.as_str().to_string();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.path().to_string())
        .unwrap_or_else(|| "/".to_string());

    // Dispatch — pick the closure / fallback / named handler. For the
    // routed mode also stamp path_params so handlers can read them.
    let (lex_req, dispatch_kind) = match dispatcher.as_ref() {
        Dispatcher::Named(name) => (lex_req, DispatchKind::Named(name.clone())),
        Dispatcher::Closure(c) => (lex_req, DispatchKind::Closure(c.clone())),
        Dispatcher::Routed { routes, fallback } => {
            let mut req_val = lex_req;
            let (closure, params) = match dispatch_route(routes, &method, &path) {
                Some((c, p)) => (c.clone(), p),
                None => (fallback.clone(), Default::default()),
            };
            stamp_path_params(&mut req_val, params);
            (req_val, DispatchKind::Closure(closure))
        }
    };

    // Run the VM on a blocking worker — same model as serve_http_fn.
    // The Lex VM is synchronous and may itself do blocking I/O
    // (file reads, DB calls), so we never run it on a tokio runtime
    // thread.
    let resp_result: Result<Value, String> = tokio::task::spawn_blocking(move || {
        let handler = DefaultHandler::new(policy).with_program(Arc::clone(&program));
        let mut vm = Vm::with_handler(&program, Box::new(handler));
        let r = match dispatch_kind {
            DispatchKind::Named(name) => vm.call(&name, vec![lex_req]),
            DispatchKind::Closure(c) => vm.invoke_closure_value(c, vec![lex_req]),
        };
        r.map_err(|e| format!("{e:?}"))
    })
    .await
    .map_err(|e| format!("h3 vm worker: {e}"))?;

    let lex_resp = match resp_result {
        Ok(v) => v,
        Err(e) => {
            // 500 the request, log the panic — same pattern as TCP.
            eprintln!("net.serve_quic: handler error: {e}");
            send_simple_error(&mut stream, 500, &format!("internal error: {e}")).await?;
            return Ok(());
        }
    };

    let (status, body_out, headers) = handler::unpack_response(&lex_resp);
    let mut resp_builder = hyper::http::Response::builder().status(status);
    for (k, v) in &headers {
        resp_builder = resp_builder.header(k, v);
    }
    let resp = resp_builder
        .body(())
        .map_err(|e| format!("h3 build response: {e}"))?;
    stream
        .send_response(resp)
        .await
        .map_err(|e| format!("h3 send_response: {e}"))?;

    // Stream bodies aren't supported on the QUIC path yet — for v1 we
    // collect to a single chunk before sending. Streaming over h3
    // would mean wiring the Lex stream iterator into a series of
    // `send_data` calls; deferred to a follow-up.
    let body_bytes = match body_out {
        handler::ResponseBodyOut::Str(s) => Bytes::from(s.into_bytes()),
        handler::ResponseBodyOut::TextChunks(chunks)
        | handler::ResponseBodyOut::BytesChunks(chunks) => {
            let mut buf = Vec::new();
            for c in chunks {
                buf.extend_from_slice(&c);
            }
            Bytes::from(buf)
        }
    };
    if !body_bytes.is_empty() {
        stream
            .send_data(body_bytes)
            .await
            .map_err(|e| format!("h3 send_data: {e}"))?;
    }
    stream
        .finish()
        .await
        .map_err(|e| format!("h3 finish: {e}"))?;
    Ok(())
}

enum DispatchKind {
    Named(String),
    Closure(Value),
}

async fn send_simple_error<S>(
    stream: &mut h3::server::RequestStream<S, Bytes>,
    status: u16,
    msg: &str,
) -> Result<(), String>
where
    S: h3::quic::BidiStream<Bytes> + Send + 'static,
    <S as h3::quic::BidiStream<Bytes>>::SendStream: Send,
    <S as h3::quic::BidiStream<Bytes>>::RecvStream: Send,
{
    let resp = hyper::http::Response::builder()
        .status(status)
        .body(())
        .map_err(|e| format!("h3 build error: {e}"))?;
    stream
        .send_response(resp)
        .await
        .map_err(|e| format!("h3 send_response: {e}"))?;
    stream
        .send_data(Bytes::copy_from_slice(msg.as_bytes()))
        .await
        .map_err(|e| format!("h3 send_data: {e}"))?;
    stream
        .finish()
        .await
        .map_err(|e| format!("h3 finish: {e}"))?;
    Ok(())
}

/// Generate a self-signed certificate + private key for the given
/// hostname (`SubjectAlternativeName`). Returns PEM-encoded bytes for
/// both, matching `from_pem_files`'s on-disk format. Used by
/// `tls.self_signed(hostname)` — intended for local development and
/// integration tests, NOT production.
pub(crate) fn self_signed_pem(hostname: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let ck = rcgen::generate_simple_self_signed(vec![hostname.to_string()])
        .map_err(|e| format!("rcgen self-signed: {e}"))?;
    let cert_pem = ck.cert.pem().into_bytes();
    let key_pem = ck.signing_key.serialize_pem().into_bytes();
    Ok((cert_pem, key_pem))
}
