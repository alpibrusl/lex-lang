//! On-disk embedding cache (#224 slice 2).
//!
//! Wraps any [`Embedder`] so the second `lex store search` query
//! doesn't pay for re-embedding every stage. The cache key is a
//! SHA-256 over `(fingerprint, text)` where `fingerprint` is the
//! caller-provided string identifying the upstream model + dim —
//! switching providers or models invalidates entries automatically.
//!
//! Layout:
//!
//! ```text
//! <root>/<first-2-hex>/<remaining-62-hex>.json
//! ```
//!
//! Each file is `{"v":[f32, ...]}` — minimal JSON so `cat` debugging
//! is sane and corrupt files surface as parse errors. The two-byte
//! sharding keeps a single directory from bloating past a few
//! thousand entries.

use crate::embedder::{EmbedError, Embedder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Filesystem-backed cache wrapping an inner embedder.
pub struct CachingEmbedder<E: Embedder> {
    inner: E,
    root: PathBuf,
    fingerprint: String,
}

impl<E: Embedder> CachingEmbedder<E> {
    /// Build a new caching embedder. `root` is created on first
    /// write; missing files are treated as cache misses.
    /// `fingerprint` should encode the upstream model identity —
    /// e.g. `"ollama:nomic-embed-text"` — so swapping providers
    /// doesn't return stale vectors of the wrong shape.
    pub fn new(inner: E, root: impl Into<PathBuf>, fingerprint: impl Into<String>) -> Self {
        Self {
            inner,
            root: root.into(),
            fingerprint: fingerprint.into(),
        }
    }

    fn key_for(&self, text: &str) -> String {
        let mut h = Sha256::new();
        h.update(self.fingerprint.as_bytes());
        h.update(b"\x1f"); // ASCII unit separator: avoids "abc"+"d" colliding with "ab"+"cd".
        h.update(text.as_bytes());
        hex::encode(h.finalize())
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let (lo, hi) = key.split_at(2);
        self.root.join(lo).join(format!("{hi}.json"))
    }

    fn read(&self, key: &str) -> Option<Vec<f32>> {
        let path = self.path_for(key);
        let bytes = std::fs::read(&path).ok()?;
        let parsed: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        Some(parsed.v)
    }

    fn write(&self, key: &str, vec: &[f32]) {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let payload = CacheEntry { v: vec.to_vec() };
        if let Ok(bytes) = serde_json::to_vec(&payload) {
            // Atomic-ish: write tmp + rename so a partial file never
            // looks valid. Errors are swallowed because the cache is
            // an optimisation, not a source of truth.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    v: Vec<f32>,
}

impl<E: Embedder> Embedder for CachingEmbedder<E> {
    fn dim(&self) -> usize { self.inner.dim() }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        // First pass: find which keys are missing, build a dense
        // list of missing texts to send upstream.
        let keys: Vec<String> = texts.iter().map(|t| self.key_for(t)).collect();
        let mut hits: Vec<Option<Vec<f32>>> = keys.iter().map(|k| self.read(k)).collect();
        let missing_idx: Vec<usize> = hits.iter().enumerate()
            .filter_map(|(i, v)| if v.is_none() { Some(i) } else { None })
            .collect();
        if !missing_idx.is_empty() {
            let missing_texts: Vec<&str> = missing_idx.iter().map(|i| texts[*i]).collect();
            let fresh = self.inner.embed_batch(&missing_texts)?;
            for (slot, vec) in missing_idx.iter().zip(fresh.iter()) {
                self.write(&keys[*slot], vec);
                hits[*slot] = Some(vec.clone());
            }
        }
        Ok(hits.into_iter().map(|v| v.expect("filled above")).collect())
    }
}

/// Pick a cache root under `<store_root>/search/embeddings/`. The
/// CLI uses this so the cache lives next to the store it indexes.
pub fn default_cache_root(store_root: &Path) -> PathBuf {
    store_root.join("search").join("embeddings")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockEmbedder;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Embedder that delegates to a Mock but counts every text it
    /// embeds, so tests can assert cache hits don't reach the inner
    /// embedder.
    struct CountingMock {
        inner: MockEmbedder,
        count: Arc<AtomicUsize>,
    }
    impl Embedder for CountingMock {
        fn dim(&self) -> usize { self.inner.dim() }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.count.fetch_add(texts.len(), Ordering::SeqCst);
            self.inner.embed_batch(texts)
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<AtomicUsize>) {
        (tempfile::tempdir().unwrap(), Arc::new(AtomicUsize::new(0)))
    }

    #[test]
    fn second_call_is_a_cache_hit() {
        let (dir, count) = fixture();
        let cache = CachingEmbedder::new(
            CountingMock { inner: MockEmbedder::new(), count: Arc::clone(&count) },
            dir.path().to_path_buf(),
            "test:v1",
        );
        let _ = cache.embed("alpha beta").unwrap();
        let n_after_first = count.load(Ordering::SeqCst);
        assert_eq!(n_after_first, 1);

        let _ = cache.embed("alpha beta").unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst), n_after_first,
            "a repeat call must not reach the inner embedder",
        );
    }

    #[test]
    fn cached_vector_matches_uncached_vector() {
        let (dir, count) = fixture();
        let cache = CachingEmbedder::new(
            CountingMock { inner: MockEmbedder::new(), count: Arc::clone(&count) },
            dir.path().to_path_buf(),
            "test:v1",
        );
        let direct = MockEmbedder::new().embed("the quick brown fox").unwrap();
        let v1 = cache.embed("the quick brown fox").unwrap();
        let v2 = cache.embed("the quick brown fox").unwrap();
        assert_eq!(v1, direct);
        assert_eq!(v2, direct);
    }

    #[test]
    fn batch_mixes_hits_and_misses_correctly() {
        let (dir, count) = fixture();
        let cache = CachingEmbedder::new(
            CountingMock { inner: MockEmbedder::new(), count: Arc::clone(&count) },
            dir.path().to_path_buf(),
            "test:v1",
        );
        let _ = cache.embed("warm").unwrap();
        count.store(0, Ordering::SeqCst);
        let _ = cache.embed_batch(&["warm", "cold", "warm", "lukewarm"]).unwrap();
        // "warm" cached, "cold" + "lukewarm" fresh → 2 inner calls.
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fingerprint_change_invalidates_cache() {
        let (dir, count) = fixture();
        let cache_a = CachingEmbedder::new(
            CountingMock { inner: MockEmbedder::new(), count: Arc::clone(&count) },
            dir.path().to_path_buf(),
            "model-a",
        );
        let cache_b = CachingEmbedder::new(
            CountingMock { inner: MockEmbedder::new(), count: Arc::clone(&count) },
            dir.path().to_path_buf(),
            "model-b",
        );
        let _ = cache_a.embed("hello").unwrap();
        count.store(0, Ordering::SeqCst);
        let _ = cache_b.embed("hello").unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1,
            "a different fingerprint must NOT serve cached entries from another model");
    }

    #[test]
    fn corrupt_cache_file_falls_back_to_fresh_embed() {
        let (dir, count) = fixture();
        let cache = CachingEmbedder::new(
            CountingMock { inner: MockEmbedder::new(), count: Arc::clone(&count) },
            dir.path().to_path_buf(),
            "test:v1",
        );
        // Prime the cache then truncate the file.
        let _ = cache.embed("token").unwrap();
        let key = cache.key_for("token");
        std::fs::write(cache.path_for(&key), b"not json").unwrap();
        count.store(0, Ordering::SeqCst);
        let _ = cache.embed("token").unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1,
            "corrupt cache entry must trigger a fresh upstream call");
    }

    #[test]
    fn default_cache_root_lives_under_store() {
        let p = default_cache_root(Path::new("/tmp/store"));
        assert!(p.ends_with("search/embeddings"));
    }
}
