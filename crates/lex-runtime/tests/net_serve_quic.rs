//! Integration tests for `net.serve_quic` / `tls.self_signed` (#496).
//!
//! Exercises an end-to-end HTTP/3 round-trip: spawn a Lex program that
//! calls `net.serve_quic` with a self-signed cert, then drive it from a
//! `quinn` + `h3` client on the same loopback. Gated behind the `quic`
//! feature — `cargo test -p lex-runtime --features quic` to run.

#![cfg(feature = "quic")]

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn spawn_lex_server(src: &str, entry: &str) {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net".to_string()].into_iter().collect::<BTreeSet<_>>();
    let entry = entry.to_string();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy.clone()).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call(&entry, vec![]);
    });
}

/// Trust-all verifier for the self-signed cert we generate inside the
/// Lex program. The integration test asserts wire-level behaviour, not
/// PKI correctness, so accepting any cert is correct.
#[derive(Debug)]
struct AcceptAll;

impl rustls::client::danger::ServerCertVerifier for AcceptAll {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn h3_get(server_port: u16, path: &str) -> Result<(u16, String), String> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAll))
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h3".to_vec()];
    tls_config.enable_early_data = false;

    let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| format!("quic client config: {e}"))?;
    let client_cfg = quinn::ClientConfig::new(Arc::new(qcc));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
        .map_err(|e| format!("client bind: {e}"))?;
    endpoint.set_default_client_config(client_cfg);

    let server_addr: SocketAddr = ([127, 0, 0, 1], server_port).into();

    let conn = endpoint
        .connect(server_addr, "localhost")
        .map_err(|e| format!("connect: {e}"))?
        .await
        .map_err(|e| format!("handshake: {e}"))?;

    let h3_conn = h3_quinn::Connection::new(conn);
    let (mut driver, mut send_req) =
        h3::client::new(h3_conn).await.map_err(|e| format!("h3 client: {e}"))?;

    let drive = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    let uri: hyper::http::Uri = format!("https://localhost{path}")
        .parse()
        .map_err(|e| format!("uri: {e}"))?;
    let req = hyper::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(())
        .map_err(|e| format!("request build: {e}"))?;

    let mut stream = send_req
        .send_request(req)
        .await
        .map_err(|e| format!("send_request: {e}"))?;
    stream.finish().await.map_err(|e| format!("finish: {e}"))?;

    let resp = stream
        .recv_response()
        .await
        .map_err(|e| format!("recv_response: {e}"))?;
    let status = resp.status().as_u16();

    let mut body = Vec::new();
    while let Some(mut chunk) = stream
        .recv_data()
        .await
        .map_err(|e| format!("recv_data: {e}"))?
    {
        use bytes::Buf as _;
        let remaining = chunk.remaining();
        body.extend_from_slice(chunk.copy_to_bytes(remaining).as_ref());
    }

    drop(send_req);
    let _ = drive.await;
    endpoint.close(0u32.into(), b"done");
    endpoint.wait_idle().await;

    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

async fn wait_for_quic_bind(port: u16, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    let mut backoff = Duration::from_millis(50);
    loop {
        // Try a one-shot h3_get; success means the server is up.
        if let Ok((_status, _)) = h3_get(port, "/__probe").await {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("HTTP/3 server on :{port} did not respond within {timeout:?}");
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_millis(500));
    }
}

/// End-to-end smoke test: serve_quic with a self-signed cert, GET via
/// h3 client, assert handler-returned body comes back. Uses a high
/// non-reserved port (UDP 18301) to avoid clashing with the TCP serve
/// tests in net_serve.rs.
#[test]
fn serve_quic_self_signed_round_trip() {
    let src = r#"
import "std.net" as net
import "std.tls" as tls
import "std.str" as str

fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  { status: 200, body: str.concat("h3-via-quic: ", req.path) }
}

fn main() -> [net] Nil {
  match tls.self_signed("localhost") {
    Ok(t) => net.serve_quic(18301, t, "handle"),
    Err(_) => (),
  }
}
"#;
    spawn_lex_server(src, "main");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        wait_for_quic_bind(18301, Duration::from_secs(10)).await;
        let (status, body) = h3_get(18301, "/hello")
            .await
            .expect("h3 get succeeds");
        assert_eq!(status, 200);
        assert_eq!(body, "h3-via-quic: /hello");
    });
}
