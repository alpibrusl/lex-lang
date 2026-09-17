//! Shared helpers for the integration tests that start a server.
//!
//! `wait_for_bind` lived as a copy-pasted function in six test files and in
//! none of the others, which is why three of them still raced a fixed sleep
//! against a server bind long after the fix existed (#805). A helper that can
//! only be adopted by copying does not get adopted by files written later.
//!
//! `tests/common/mod.rs` rather than `tests/common.rs`: the directory form is
//! not compiled as its own test binary, so this does not appear as an empty
//! test target.

use std::net::{TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

/// Poll-connect to `port` until the listener accepts a TCP connection or the
/// deadline expires.
///
/// Replaces a fixed sleep so a test passes whether bind takes 5 ms (plain
/// HTTP) or 500 ms (TLS with a cold cert load) without slowing the fast path.
/// A fixed 200 ms budget is least reliable exactly where it matters most — a
/// shared CI runner under `--test-threads=1` — and when it runs out the panic
/// lands on the `connect`, not on anything the test is about, so an unrelated
/// change looks guilty.
///
/// Panics with the port and the budget when the deadline passes, so a genuine
/// "the server never started" failure is still legible.
pub fn wait_for_bind(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(20);
    loop {
        if let Ok(s) = TcpStream::connect_timeout(
            &("127.0.0.1", port).to_socket_addrs().unwrap().next().unwrap(),
            Duration::from_millis(200),
        ) {
            drop(s);
            return;
        }
        if Instant::now() >= deadline {
            panic!("server on :{port} did not bind within {timeout:?}");
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(200));
    }
}
