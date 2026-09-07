//! The `net.serve_ws*` family can bind an interface other than
//! loopback (#719).
//!
//! Every WS server bound `127.0.0.1` unconditionally, so a
//! containerised deployment could not accept cross-container
//! connections at all — ev-fleet's lex-csms was running a `socat`
//! sidecar (`socat TCP-LISTEN:9001,fork,reuseaddr TCP:127.0.0.1:9000`)
//! purely to republish the loopback listener on the container
//! interface.
//!
//! The issue names `serve_ws_fn_actor`; the same line was in all four
//! (`serve_ws`, `serve_ws_fn`, `serve_ws_fn_auth`, `serve_ws_fn_actor`),
//! so all four are fixed and this suite pins the resolver they share.
//!
//! These test the *resolver*, not a live listener: binding a real
//! socket in a unit test is flaky under parallel runs and says nothing
//! extra — the bind call takes whatever this returns.

use lex_runtime::ws::ws_bind_host;

fn env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Run `f` with `LEX_WS_HOST` set to `v`, restoring it afterwards.
///
/// `std::env` is process-global and these tests share one process, so
/// they serialise on a lock rather than racing.
///
/// **A closure, not a guard**, and that is the whole point. The first
/// draft handed back an RAII guard, which made this compile and hang
/// for ever:
///
/// ```ignore
/// let _g = HostVar::set(Some("0.0.0.0"));   // holds the lock
/// let _g = HostVar::set(Some("10.1.2.3"));  // shadows; first is STILL alive
/// ```
///
/// `let _g = …` twice in one scope shadows the binding but does not
/// drop the first guard, so the second call blocks on a
/// `std::sync::Mutex` the same thread already holds — and
/// `--test-threads=1` turns one hung test into a hung suite. Taking
/// the body as a closure makes the lock's scope the call itself, so
/// the mistake cannot be written.
///
/// The env var is restored even if `f` panics: the guard here is
/// internal and never escapes, so nothing can hold it across a second
/// acquisition.
fn with_host<T>(v: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var("LEX_WS_HOST").ok();
    match v {
        Some(h) => std::env::set_var("LEX_WS_HOST", h),
        None => std::env::remove_var("LEX_WS_HOST"),
    }
    let restore = || match &prior {
        Some(p) => std::env::set_var("LEX_WS_HOST", p),
        None => std::env::remove_var("LEX_WS_HOST"),
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    restore();
    match out {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}

/// **The default does not move.** Widening what an existing program
/// listens on, on upgrade and without anybody asking, would be a
/// security regression dressed as a bug fix: a server only ever
/// reachable from localhost must not silently become reachable from the
/// network.
#[test]
fn the_default_is_still_loopback() {
    with_host(None, || assert_eq!(ws_bind_host(), "127.0.0.1"));
}

/// The escape hatch that fixes a deployment without touching the
/// program — the same shape as `LEX_NET_INLINE_VM` and `LEX_NET_HTTP2`
/// on the legacy HTTP path.
#[test]
fn the_env_var_overrides_it() {
    with_host(Some("0.0.0.0"), || assert_eq!(ws_bind_host(), "0.0.0.0"));
    with_host(Some("10.1.2.3"), || assert_eq!(ws_bind_host(), "10.1.2.3"));
}

/// An empty or whitespace-only value is somebody who set the variable
/// and meant nothing by it. Binding `""` would fail at the syscall with
/// a message about an address nobody typed, so it falls back instead.
#[test]
fn a_blank_value_falls_back_rather_than_binding_nothing() {
    for blank in ["", "   ", "\t"] {
        with_host(Some(blank), || {
            assert_eq!(
                ws_bind_host(),
                "127.0.0.1",
                "a blank LEX_WS_HOST ({blank:?}) must not become the bind address"
            )
        });
    }
}
