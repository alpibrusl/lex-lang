// Negative control: nothing is listening on this port, so wait_for_bind must
// panic with a legible message rather than hang or pass.
mod common;
use common::wait_for_bind;
use std::time::{Duration, Instant};

#[test]
fn unbound_port_panics_with_the_port_and_budget() {
    let t0 = Instant::now();
    let r = std::panic::catch_unwind(|| wait_for_bind(59999, Duration::from_millis(600)));
    let msg = r.expect_err("must panic when nothing binds")
        .downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("59999"), "message must name the port: {msg}");
    assert!(msg.contains("did not bind"), "message must say what failed: {msg}");
    assert!(t0.elapsed() >= Duration::from_millis(500), "must actually wait the budget");
    assert!(t0.elapsed() < Duration::from_secs(5), "must not overshoot: {:?}", t0.elapsed());
}
