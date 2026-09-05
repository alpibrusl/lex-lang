//! HTTP client behind `net.get` / `net.post` / `http.*`: ureq agents, request building, response decoding and `http.stream_lines`.

use super::*;

/// HTTP/1.1 client backed by `ureq` + `rustls`. Accepts both
/// `http://` and `https://` URLs. Returns `Result[Str, Str]` as a
/// Lex `Value::Variant`. The earlier hand-rolled HTTP/1.0 client
/// was plain-TCP only — most public APIs are HTTPS, so the demo
/// could fetch `example.com` but not `wttr.in` or `api.github.com`.
pub(super) fn http_request(method: &str, url: &str, body: Option<&str>) -> Value {
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
pub(super) fn http_stream_agent() -> ureq::Agent {
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
pub(super) fn http_agent(timeout_ms: Option<u64>) -> ureq::Agent {
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
pub(super) fn http_error_value(e: ureq::Error) -> Value {
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

pub(super) fn http_decode_err(msg: String) -> Value {
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
pub(super) fn http_send_simple(
    method: &str,
    url: &str,
    body: Option<Vec<u8>>,
    content_type: &str,
    timeout_ms: Option<u64>,
) -> Value {
    http_send_full(method, url, body, content_type, &[], timeout_ms)
}

pub(super) fn http_send_full(
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

pub(super) fn collect_response_headers(
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
pub(super) fn http_send_record(handler: &DefaultHandler, req: &indexmap::IndexMap<smol_str::SmolStr, Value>) -> Value {
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

/// Streaming HTTP POST that yields the response body line-by-line as a lazy
/// `Stream[Str]` (#683). Intended for LLM provider APIs and other SSE/NDJSON
/// endpoints. Connection errors at request time → `Err(Str)`.
///
/// Truly incremental: ureq 3.3's `Body::into_reader()` gives a `BodyReader`
/// (impl `io::Read`), so the returned `Stream[Str]` is a lazy line iterator
/// directly over the socket. Each `stream.next` reads exactly the next line on
/// demand — an endpoint that holds the connection open and emits events over
/// time is consumed event-by-event instead of blocking until the server closes
/// (the old `read_to_vec()` path buffered the whole body and hung on open-ended
/// SSE). Because reads only happen when the consumer pulls, there's no extra
/// buffering; a mid-stream read error / close simply ends the stream.
pub(super) fn http_stream_lines_impl(handler: &DefaultHandler, url: &str, headers_val: &Value, body: &str) -> Value {
    let body_bytes = body.as_bytes().to_vec();
    // 10-minute body-read timeout — local models (Ollama, vLLM) can take
    // several minutes between events on long thinking traces.
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
            use std::io::BufRead;
            let reader = std::io::BufReader::new(resp.into_body().into_reader());
            // Lazy: `map_while(ok)` stops at the first read error / EOF; the
            // \uXXXX un-escaping preserves the pre-#683 decoded-text contract.
            let lines = reader
                .lines()
                .map_while(Result::ok)
                .map(|l| decode_unicode_escapes(&l));
            let handle = handler.register_stream(lines);
            ok(stream_handle_value(handle))
        }
        Err(e) => err(Value::Str(format!("http.stream_lines: {e}").into())),
    }
}
