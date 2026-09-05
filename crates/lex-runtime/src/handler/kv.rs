//! `kv` effect: the LRU-bounded `sled` handle registry behind opaque `Kv` handles.

use super::*;

pub(super) fn expect_kv_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected Kv handle (Int), got {other:?}")),
        None => Err("missing Kv argument".into()),
    }
}

/// Process-wide registry of open `Kv` handles. Each `kv.open` allocates
/// a new u64 handle via [`next_kv_handle`] and stores the `sled::Db`
/// here; subsequent ops fetch by handle. `kv.close` removes the entry.
///
/// Capped at [`MAX_KV_HANDLES`] to prevent leaks from long-running
/// programs that open many short-lived stores without calling
/// `kv.close`. On insert at cap, the least-recently-used entry is
/// dropped (closing its `sled::Db`); subsequent ops on the evicted
/// handle return the standard "closed or unknown Kv handle" error.
/// Any access (`get`, `put`, `delete`, `contains`, `list_prefix`)
/// touches the LRU order.
pub(super) fn kv_registry() -> &'static Mutex<KvRegistry> {
    static REGISTRY: OnceLock<Mutex<KvRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(KvRegistry::with_capacity(MAX_KV_HANDLES)))
}

/// Maximum number of `kv.open` handles kept alive at once. Past this
/// cap, the least-recently-used handle is evicted on each new open.
/// Sized so that pathological "open and forget" programs are bounded
/// without breaking real-world programs that intentionally keep one or
/// two long-lived stores open.
pub(super) const MAX_KV_HANDLES: usize = 256;

/// LRU-bounded set of open `sled::Db` instances keyed by `u64` handle.
/// Built on `IndexMap` for O(1) insert / remove / lookup with
/// insertion-order traversal — touching an entry just shift-moves it
/// to the back, evictions pop from the front.
pub(crate) struct KvRegistry {
    pub(super) entries: indexmap::IndexMap<u64, sled::Db>,
    pub(super) cap: usize,
}

impl KvRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    /// Insert a freshly-opened db. If we're already at cap, evict the
    /// LRU entry first; the dropped `sled::Db` closes its files.
    pub(crate) fn insert(&mut self, handle: u64, db: sled::Db) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, db);
    }

    /// Look up a handle, marking it most-recently-used on hit.
    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<&sled::Db> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle)
    }

    /// Explicit `kv.close`: drop the handle if present.
    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.entries.len() }
}

pub(super) fn next_kv_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod kv_registry_tests {
    use super::KvRegistry;

    /// Spin up an isolated `sled::Db` in a temp dir. Each call gets a
    /// unique path so concurrent tests don't collide on the lockfile.
    fn fresh_db(tag: &str) -> sled::Db {
        let dir = std::env::temp_dir().join(format!(
            "lex-kv-reg-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        sled::open(&dir).expect("sled open")
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut r = KvRegistry::with_capacity(4);
        r.insert(1, fresh_db("a"));
        assert!(r.touch_get(1).is_some());
        assert!(r.touch_get(2).is_none());
    }

    #[test]
    fn cap_evicts_lru_on_overflow() {
        // cap=2: insert 1, 2; touch 1 (now MRU); insert 3 → 2 evicted.
        let mut r = KvRegistry::with_capacity(2);
        r.insert(1, fresh_db("c1"));
        r.insert(2, fresh_db("c2"));
        let _ = r.touch_get(1);
        r.insert(3, fresh_db("c3"));
        assert!(r.touch_get(1).is_some(), "1 was MRU, should survive");
        assert!(r.touch_get(2).is_none(), "2 was LRU, should be evicted");
        assert!(r.touch_get(3).is_some(), "3 just inserted, should survive");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn cap_with_no_touches_evicts_in_insertion_order() {
        // cap=2: insert 1, 2, 3 with no touches → 1 evicted (FIFO).
        let mut r = KvRegistry::with_capacity(2);
        r.insert(10, fresh_db("f1"));
        r.insert(20, fresh_db("f2"));
        r.insert(30, fresh_db("f3"));
        assert!(r.touch_get(10).is_none());
        assert!(r.touch_get(20).is_some());
        assert!(r.touch_get(30).is_some());
    }

    #[test]
    fn remove_drops_entry() {
        let mut r = KvRegistry::with_capacity(4);
        r.insert(1, fresh_db("r1"));
        r.remove(1);
        assert!(r.touch_get(1).is_none());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn remove_unknown_handle_is_noop() {
        let mut r = KvRegistry::with_capacity(4);
        r.insert(1, fresh_db("u1"));
        r.remove(999);
        assert!(r.touch_get(1).is_some());
    }

    #[test]
    fn many_inserts_stay_bounded_at_cap() {
        // Exhaust the cap to confirm the registry never grows past it,
        // even under sustained churn.
        let cap = 8;
        let mut r = KvRegistry::with_capacity(cap);
        for i in 0..(cap as u64 * 3) {
            r.insert(i, fresh_db(&format!("b{i}")));
            assert!(r.len() <= cap);
        }
        assert_eq!(r.len(), cap);
    }
}
