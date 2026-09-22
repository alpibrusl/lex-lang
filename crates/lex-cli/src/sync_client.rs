//! HTTP client for the op-sync endpoints (`lex op push` / `lex op pull`)
//! with failure classification and bounded retry (#971).
//!
//! Before this module every sync call read the response body as JSON
//! *before* looking at the status, so a reverse proxy's empty-bodied
//! `502 Bad Gateway` (what Caddy returns while lex-hub is cold) surfaced as
//! `decoding /v1/stages/fetch response: json: EOF while parsing a value at
//! line 1 column 0` — an error that names neither the status nor the cause.
//! Here each failure is classified, in order, as:
//!
//! 1. **transport** — the request never got a response (DNS, refused, reset);
//! 2. **timeout** — no complete response inside the per-request deadline;
//! 3. **auth** — HTTP 401;
//! 4. **status** — any other non-2xx, shown with a truncated body;
//! 5. **empty body** — a 2xx with zero bytes (a proxy/upstream hiccup);
//! 6. **body read** — the connection dropped mid-body;
//! 7. **parse** — a genuine, non-empty body that isn't the expected JSON.
//!
//! Every message names the method and endpoint. For requests the caller marks
//! as [`Retry::Idempotent`], the transient kinds (timeouts, 502/503/504,
//! empty bodies, mid-body drops, connection resets) are retried with
//! exponential backoff before giving up.

use serde::de::DeserializeOwned;
use std::fmt;
use std::time::Duration;

/// Hard cap on a sync response body. ureq defaults to 10MB, which
/// `/v1/ops/since` blew past on a large tenant; pagination keeps pages
/// small, this guards against a single oversized page/blob.
pub(crate) const SYNC_BODY_LIMIT: u64 = 512 * 1024 * 1024;

/// How much of a non-2xx / unparseable body to quote in an error.
const BODY_SNIPPET: usize = 300;

/// Whether a request may be re-sent after a transient failure.
///
/// Only mark a request `Idempotent` when sending it twice is harmless:
/// read-only fetches, or writes the server dedups by content-addressed id
/// (`/v1/ops/batch` skips an existing `op_id`; `/v1/stages/batch`,
/// `/v1/intents/batch`, `/v1/issues/batch` and `/v1/locks/batch` all key
/// storage by the content hash — see `lex-api/src/sync_http.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    Idempotent,
    Once,
}

/// Retry/timeout knobs. [`RetryPolicy::from_env`] is what the CLI uses.
#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
    /// Retries after the first attempt (so `max_retries + 1` attempts total).
    pub max_retries: u32,
    /// Delay before the first retry; doubles each time, capped at `max_delay`.
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// End-to-end deadline for one attempt (connect → last body byte).
    pub timeout: Option<Duration>,
}

impl RetryPolicy {
    /// Defaults: 5 retries at 1s, 2s, 4s, 8s, 16s (31s of backoff — enough to
    /// ride out the >25s hub cold start measured in #971) and a 120s
    /// per-attempt deadline. Overridable with `LEX_SYNC_RETRIES` and
    /// `LEX_SYNC_TIMEOUT_SECS` (`0` disables the deadline).
    pub(crate) fn from_env() -> Self {
        let env_u64 = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let max_retries = env_u64("LEX_SYNC_RETRIES")
            .map(|n| n.min(20) as u32)
            .unwrap_or(5);
        let timeout = match env_u64("LEX_SYNC_TIMEOUT_SECS") {
            Some(0) => None,
            Some(s) => Some(Duration::from_secs(s)),
            None => Some(Duration::from_secs(120)),
        };
        RetryPolicy {
            max_retries,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(16),
            timeout,
        }
    }

    fn delay_for(&self, retry_index: u32) -> Duration {
        let factor = 1u32.checked_shl(retry_index.min(16)).unwrap_or(u32::MAX);
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// A classified sync-request failure. `Display` is the user-facing message.
#[derive(Debug)]
pub(crate) enum SyncError {
    Transport {
        endpoint: String,
        detail: String,
        retryable: bool,
    },
    Timeout {
        endpoint: String,
        timeout: Option<Duration>,
    },
    Auth {
        endpoint: String,
    },
    Status {
        endpoint: String,
        status: u16,
        body: String,
    },
    EmptyBody {
        endpoint: String,
        status: u16,
    },
    BodyRead {
        endpoint: String,
        status: u16,
        detail: String,
    },
    Parse {
        endpoint: String,
        status: u16,
        detail: String,
        body: String,
    },
}

impl SyncError {
    fn retryable(&self) -> bool {
        match self {
            SyncError::Transport { retryable, .. } => *retryable,
            SyncError::Timeout { .. }
            | SyncError::EmptyBody { .. }
            | SyncError::BodyRead { .. } => true,
            SyncError::Status { status, .. } => matches!(status, 502..=504),
            SyncError::Auth { .. } | SyncError::Parse { .. } => false,
        }
    }

    /// Short cause for the "retrying after …" progress line.
    fn cause(&self) -> String {
        match self {
            SyncError::Transport { detail, .. } => format!("transport error ({detail})"),
            SyncError::Timeout { .. } => "timeout".into(),
            SyncError::Status { status, .. } => format!("HTTP {status}"),
            SyncError::EmptyBody { .. } => "empty response body".into(),
            SyncError::BodyRead { .. } => "connection dropped mid-body".into(),
            SyncError::Auth { .. } => "HTTP 401".into(),
            SyncError::Parse { .. } => "invalid JSON".into(),
        }
    }

    /// The HTTP status, when the server answered with one.
    #[cfg(test)]
    fn status(&self) -> Option<u16> {
        match self {
            SyncError::Status { status, .. }
            | SyncError::EmptyBody { status, .. }
            | SyncError::BodyRead { status, .. }
            | SyncError::Parse { status, .. } => Some(*status),
            SyncError::Auth { .. } => Some(401),
            _ => None,
        }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Transport {
                endpoint, detail, ..
            } => write!(
                f,
                "{endpoint}: transport failure, no response received ({detail}) — \
                 check the remote URL and that the hub is reachable"
            ),
            SyncError::Timeout { endpoint, timeout } => {
                let secs = timeout
                    .map(|t| format!(" after {}s", t.as_secs()))
                    .unwrap_or_default();
                write!(
                    f,
                    "{endpoint}: timed out{secs} waiting for the hub — it may be cold-starting; \
                     retry, or raise LEX_SYNC_TIMEOUT_SECS"
                )
            }
            SyncError::Auth { endpoint } => write!(
                f,
                "{endpoint}: remote requires auth (HTTP 401) — set LEXHUB_TOKEN or pass --token"
            ),
            SyncError::Status {
                endpoint,
                status,
                body,
            } => {
                let hint = match status {
                    502..=504 => {
                        " — the hub (or the proxy in front of it) is unavailable or \
                                  still warming up; retry shortly"
                    }
                    _ => "",
                };
                let body = if body.is_empty() {
                    "(empty body)".to_string()
                } else {
                    body.clone()
                };
                write!(f, "{endpoint}: server returned HTTP {status}{hint}: {body}")
            }
            SyncError::EmptyBody { endpoint, status } => write!(
                f,
                "{endpoint}: HTTP {status} with an empty body where JSON was expected — \
                 usually a proxy or upstream hiccup (e.g. a hub cold start), not a protocol \
                 error; retry"
            ),
            SyncError::BodyRead {
                endpoint,
                status,
                detail,
            } => write!(
                f,
                "{endpoint}: HTTP {status}, but the connection failed while reading the body \
                 ({detail}) — the response was truncated; retry"
            ),
            SyncError::Parse {
                endpoint,
                status,
                detail,
                body,
            } => write!(
                f,
                "{endpoint}: HTTP {status} body is not the expected JSON ({detail}); \
                 body starts: {body} — client/server version mismatch?"
            ),
        }
    }
}

impl std::error::Error for SyncError {}

/// Truncate a body for display: lossy UTF-8, whitespace-collapsed ends,
/// at most [`BODY_SNIPPET`] chars.
fn snippet(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    let mut out: String = s.chars().take(BODY_SNIPPET).collect();
    if s.chars().count() > BODY_SNIPPET {
        out.push_str(&format!("… ({} bytes total)", bytes.len()));
    }
    out
}

fn classify_transport(endpoint: &str, e: ureq::Error, timeout: Option<Duration>) -> SyncError {
    use std::io::ErrorKind;
    match e {
        ureq::Error::Timeout(_) => SyncError::Timeout {
            endpoint: endpoint.into(),
            timeout,
        },
        ureq::Error::Io(io) if io.kind() == ErrorKind::TimedOut => SyncError::Timeout {
            endpoint: endpoint.into(),
            timeout,
        },
        ureq::Error::Io(io) => {
            // A reset/abort/EOF means the peer (often a restarting upstream)
            // dropped us — transient. Refused means nothing is listening
            // there — almost always a wrong URL, so don't spin on it.
            let retryable = matches!(
                io.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::BrokenPipe
                    | ErrorKind::Interrupted
            );
            SyncError::Transport {
                endpoint: endpoint.into(),
                detail: io.to_string(),
                retryable,
            }
        }
        other => SyncError::Transport {
            endpoint: endpoint.into(),
            detail: other.to_string(),
            retryable: false,
        },
    }
}

/// One attempt: send, then classify status → body → JSON.
fn attempt<T: DeserializeOwned>(
    url: &str,
    endpoint: &str,
    body: Option<&str>,
    token: Option<&str>,
    policy: &RetryPolicy,
) -> Result<T, SyncError> {
    let sent = match body {
        Some(b) => crate::op::with_auth(ureq::post(url), token)
            .config()
            .timeout_global(policy.timeout)
            .build()
            .header("Content-Type", "application/json")
            .send(b),
        None => crate::op::with_auth(ureq::get(url), token)
            .config()
            .timeout_global(policy.timeout)
            .build()
            .call(),
    };
    let resp = sent.map_err(|e| classify_transport(endpoint, e, policy.timeout))?;
    let status = resp.status().as_u16();
    let read = resp
        .into_body()
        .with_config()
        .limit(SYNC_BODY_LIMIT)
        .read_to_vec();
    if status == 401 {
        return Err(SyncError::Auth {
            endpoint: endpoint.into(),
        });
    }
    let bytes = match read {
        Ok(b) => b,
        Err(e) if !(200..300).contains(&status) => {
            // The status already tells the story; the body is a bonus.
            return Err(SyncError::Status {
                endpoint: endpoint.into(),
                status,
                body: format!("(body unreadable: {e})"),
            });
        }
        Err(ureq::Error::Timeout(_)) => {
            return Err(SyncError::Timeout {
                endpoint: endpoint.into(),
                timeout: policy.timeout,
            })
        }
        Err(e) => {
            return Err(SyncError::BodyRead {
                endpoint: endpoint.into(),
                status,
                detail: e.to_string(),
            })
        }
    };
    if !(200..300).contains(&status) {
        return Err(SyncError::Status {
            endpoint: endpoint.into(),
            status,
            body: snippet(&bytes),
        });
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(SyncError::EmptyBody {
            endpoint: endpoint.into(),
            status,
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| SyncError::Parse {
        endpoint: endpoint.into(),
        status,
        detail: e.to_string(),
        body: snippet(&bytes),
    })
}

/// Send a sync request to `<remote><path>` and decode the JSON response.
/// `body = Some(json)` POSTs it; `None` GETs. See the module docs for the
/// error taxonomy and retry rules.
pub(crate) fn request_json<T: DeserializeOwned>(
    remote: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
    retry: Retry,
    policy: &RetryPolicy,
) -> Result<T, SyncError> {
    let url = format!("{}{}", remote.trim_end_matches('/'), path);
    // Name the endpoint without the query string's cursor noise.
    let route = path.split('?').next().unwrap_or(path);
    let endpoint = format!(
        "{} {}{route}",
        if body.is_some() { "POST" } else { "GET" },
        remote.trim_end_matches('/'),
    );
    let max_retries = if retry == Retry::Idempotent {
        policy.max_retries
    } else {
        0
    };
    let mut retries = 0u32;
    loop {
        match attempt(&url, &endpoint, body, token, policy) {
            Ok(v) => return Ok(v),
            Err(e) if e.retryable() && retries < max_retries => {
                let wait = policy.delay_for(retries);
                retries += 1;
                eprintln!(
                    "{endpoint}: {} — retrying in {:.1}s (retry {retries}/{max_retries})",
                    e.cause(),
                    wait.as_secs_f64(),
                );
                std::thread::sleep(wait);
            }
            Err(e) => {
                if retries > 0 {
                    eprintln!("{endpoint}: giving up after {} attempts", retries + 1);
                }
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A scripted stub: the n-th request gets `script[min(n, last)]`.
    /// Returns the base URL and a counter of requests served.
    fn stub(script: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let port = server.server_addr().to_ip().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let n = h.fetch_add(1, Ordering::SeqCst);
                let (status, body) = script[n.min(script.len() - 1)];
                let _ =
                    req.respond(tiny_http::Response::from_string(body).with_status_code(status));
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    fn fast(retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries: retries,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            timeout: Some(Duration::from_secs(10)),
        }
    }

    fn fetch(remote: &str, retry: Retry, p: &RetryPolicy) -> Result<serde_json::Value, SyncError> {
        request_json(
            remote,
            "/v1/stages/fetch",
            Some(r#"{"ids":[]}"#),
            None,
            retry,
            p,
        )
    }

    #[test]
    fn retries_502_then_succeeds() {
        let (url, hits) = stub(vec![(502, ""), (200, r#"{"stages":[]}"#)]);
        let v = fetch(&url, Retry::Idempotent, &fast(3)).expect("second attempt succeeds");
        assert_eq!(v, serde_json::json!({ "stages": [] }));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn empty_200_body_is_retried_then_named() {
        // An empty 200 then a real one: retried transparently.
        let (url, hits) = stub(vec![(200, ""), (200, r#"{"stages":[]}"#)]);
        fetch(&url, Retry::Idempotent, &fast(3)).expect("retry recovers from empty body");
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // Always empty: the final error says "empty body", not a JSON EOF.
        let (url, hits) = stub(vec![(200, "")]);
        let e = fetch(&url, Retry::Idempotent, &fast(2)).unwrap_err();
        assert!(
            matches!(e, SyncError::EmptyBody { status: 200, .. }),
            "{e:?}"
        );
        let msg = e.to_string();
        assert!(
            msg.contains(&format!("POST {url}/v1/stages/fetch")),
            "{msg}"
        );
        assert!(msg.contains("empty body"), "{msg}");
        assert!(!msg.contains("EOF while parsing"), "{msg}");
        assert_eq!(hits.load(Ordering::SeqCst), 3, "1 attempt + 2 retries");
    }

    #[test]
    fn persistent_502_reports_status_not_json_eof() {
        let (url, hits) = stub(vec![(502, "")]);
        let e = fetch(&url, Retry::Idempotent, &fast(2)).unwrap_err();
        assert_eq!(e.status(), Some(502));
        let msg = e.to_string();
        assert!(
            msg.contains("HTTP 502") && msg.contains("/v1/stages/fetch"),
            "{msg}"
        );
        assert!(msg.contains("warming up"), "{msg}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn non_idempotent_is_not_retried() {
        let (url, hits) = stub(vec![(503, "busy"), (200, "{}")]);
        let e = fetch(&url, Retry::Once, &fast(3)).unwrap_err();
        assert_eq!(e.status(), Some(503));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn client_error_is_not_retried_and_quotes_body() {
        let (url, hits) = stub(vec![(422, r#"{"error":"MissingParent"}"#)]);
        let e = fetch(&url, Retry::Idempotent, &fast(3)).unwrap_err();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(e.to_string().contains("MissingParent"), "{e}");
    }

    #[test]
    fn auth_failure_is_named() {
        let (url, _) = stub(vec![(401, "")]);
        let e = fetch(&url, Retry::Idempotent, &fast(3)).unwrap_err();
        assert!(matches!(e, SyncError::Auth { .. }));
        assert!(e.to_string().contains("LEXHUB_TOKEN"));
    }

    #[test]
    fn genuine_parse_error_is_distinguished_and_quoted() {
        let (url, hits) = stub(vec![(200, "<html>oops</html>")]);
        let e = fetch(&url, Retry::Idempotent, &fast(3)).unwrap_err();
        assert!(matches!(e, SyncError::Parse { .. }), "{e:?}");
        assert!(e.to_string().contains("<html>oops</html>"), "{e}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a real parse error is not transient"
        );
    }

    #[test]
    fn long_bodies_are_truncated() {
        let big = "x".repeat(10_000);
        let s = snippet(big.as_bytes());
        assert!(s.len() < 400 && s.contains("10000 bytes total"), "{s}");
    }

    #[test]
    fn timeout_is_classified_and_retried() {
        // A listener that accepts but never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&accepted);
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in listener.incoming() {
                a.fetch_add(1, Ordering::SeqCst);
                held.push(s);
            }
        });
        let p = RetryPolicy {
            timeout: Some(Duration::from_millis(200)),
            ..fast(1)
        };
        let e = fetch(&format!("http://127.0.0.1:{port}"), Retry::Idempotent, &p).unwrap_err();
        assert!(matches!(e, SyncError::Timeout { .. }), "{e:?}");
        assert!(e.to_string().contains("timed out"), "{e}");
        assert_eq!(accepted.load(Ordering::SeqCst), 2, "timeout retried once");
    }

    #[test]
    fn refused_connection_is_transport_and_not_retried() {
        // Bind then drop to get a port with nothing listening.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = fetch(
            &format!("http://127.0.0.1:{port}"),
            Retry::Idempotent,
            &fast(3),
        )
        .unwrap_err();
        assert!(
            matches!(
                e,
                SyncError::Transport {
                    retryable: false,
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(e.to_string().contains("transport failure"), "{e}");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy {
            max_retries: 9,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(16),
            timeout: None,
        };
        let d: Vec<u64> = (0..6).map(|i| p.delay_for(i).as_secs()).collect();
        assert_eq!(d, vec![1, 2, 4, 8, 16, 16]);
    }
}
