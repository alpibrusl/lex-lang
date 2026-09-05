//! `net` UDP sockets: the capped socket registry, datagram values and the `--allow-net-host` check for datagram destinations.

use super::*;

// ── UDP datagrams (#760) ─────────────────────────────────────────────
//
// Handle-based, mirroring `sql.open`: an Int index into a process-global
// registry, because a socket has a lifetime the Lex value model has no
// way to carry.
//
// Capped like the SQL registry. A leaked socket is a leaked file
// descriptor, and a program that opens them in a loop should fail with a
// message naming the cause rather than exhausting the process's fds and
// failing somewhere unrelated.
pub(super) const MAX_UDP_HANDLES: usize = 256;

pub(super) fn udp_registry() -> &'static Mutex<std::collections::HashMap<u64, std::net::UdpSocket>> {
    static REGISTRY: OnceLock<Mutex<std::collections::HashMap<u64, std::net::UdpSocket>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

pub(super) fn next_udp_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Run `f` against the socket behind `handle`.
///
/// The socket is used through the registry lock rather than cloned out of
/// it, so a `udp_close` racing a `udp_recv` cannot pull the fd out from
/// under a blocked read.
pub(super) fn with_udp<T>(
    handle: i64,
    op: &str,
    f: impl FnOnce(&std::net::UdpSocket) -> Result<T, String>,
) -> Result<T, String> {
    let reg = udp_registry().lock().unwrap();
    match reg.get(&(handle as u64)) {
        Some(sock) => f(sock),
        None => Err(format!("{op}: no open socket with handle {handle} (closed, or never opened?)")),
    }
}

pub(super) fn udp_datagram_value(data: Vec<u8>, addr: std::net::SocketAddr) -> Value {
    let mut rec = indexmap::IndexMap::new();
    rec.insert("data".into(), Value::Bytes(data));
    rec.insert("host".into(), Value::Str(addr.ip().to_string().into()));
    rec.insert("port".into(), Value::Int(i64::from(addr.port())));
    Value::record_dynamic(rec)
}

impl DefaultHandler {
    /// The `--allow-net-host` gate, applied to a datagram destination.
    ///
    /// `ensure_host_allowed` takes a URL; a UDP destination is already a
    /// bare host, so this is the same check without the parse. Kept as its
    /// own function so the error names the datagram rather than pretending
    /// a URL was involved.
    pub(super) fn ensure_udp_dest_allowed(&self, host: &str) -> Result<(), String> {
        if self.policy.allow_net_host.is_empty() { return Ok(()); }
        if self.policy.allow_net_host.iter().any(|h| host == h) {
            Ok(())
        } else {
            Err(format!(
                "net.udp_send to `{host}` not in --allow-net-host {:?}",
                self.policy.allow_net_host,
            ))
        }
    }
}
