//! `redis` effect (#533): the LRU-bounded connection registry behind
//! opaque `ConnRedis` handles. `ConnRedis` is an opaque Int handle into
//! `RedisRegistry`. Each `redis.connect` allocates a new handle via
//! `next_redis_handle` and stores the open `redis::Connection` plus the
//! original URL (needed to open dedicated pub/sub connections for
//! `subscribe` / `psubscribe`). LRU-bounded at `MAX_REDIS_HANDLES` to
//! avoid leaks from programs that open many short-lived connections
//! without calling `redis.close`.

use super::*;

/// Per-handle state: the live synchronous connection and the URL it
/// was opened from. The URL is kept so `subscribe`/`psubscribe` can
/// open a fresh dedicated connection (Redis forbids non-Pub/Sub
/// commands on a subscribed connection).
pub(super) struct RedisEntry {
    pub(super) url: String,
    pub(super) conn: redis::Connection,
}

pub(super) struct RedisRegistry {
    pub(super) entries: indexmap::IndexMap<u64, RedisEntry>,
    pub(super) cap: usize,
}

impl RedisRegistry {
    pub(super) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    pub(super) fn insert(&mut self, handle: u64, entry: RedisEntry) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, entry);
    }

    pub(super) fn touch_get_mut(&mut self, handle: u64) -> Option<&mut RedisEntry> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get_mut(&handle)
    }

    /// Return the URL for a handle without touching LRU order. Used by
    /// `subscribe`/`psubscribe` to open a dedicated connection.
    pub(super) fn get_url(&self, handle: u64) -> Option<String> {
        self.entries.get(&handle).map(|e| e.url.clone())
    }

    pub(super) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }
}

pub(super) fn redis_registry() -> &'static Mutex<RedisRegistry> {
    static REGISTRY: OnceLock<Mutex<RedisRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(RedisRegistry::with_capacity(MAX_REDIS_HANDLES)))
}

pub(super) const MAX_REDIS_HANDLES: usize = 256;

pub(super) fn next_redis_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

pub(super) fn expect_redis_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected ConnRedis (Int), got {other:?}")),
        None => Err("missing ConnRedis argument".into()),
    }
}
