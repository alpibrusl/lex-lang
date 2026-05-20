//! Per-request bump-allocator arena (#463 scaffolding).
//!
//! The minimal lifecycle machinery for request-scoped allocations.
//! See `docs/design/jit-roadmap.md` for the broader plan and
//! issue #463 for the perf rationale.
//!
//! ## Status: scaffolding only
//!
//! This module is wired through the `EffectHandler` trait so the VM
//! can call `enter_request_scope` / `exit_request_scope` at the
//! right boundaries — but the arena is **not yet plumbed into
//! `Value` allocations**. Today every `MakeRecord`, `MakeList`,
//! `Str` still goes through the global allocator. The arena gets
//! created and dropped at each request boundary as a no-op proof
//! that the lifetime machinery works, ready for a follow-on slice
//! to route actual allocations.
//!
//! ## Why scaffolding-first
//!
//! Routing `Value`'s heap parts (`Box<IndexMap<…>>`,
//! `VecDeque<Value>`, `Vec<u8>`, …) through the arena requires
//! either a lifetime parameter on `Value` (massive churn — every
//! one of the ~60 `as_int` / `as_str` / `as_bool` call sites in
//! the codebase) or an arena-id tag on every heap allocation
//! (`Drop` impl must dispatch to the right allocator). Both are
//! sized in months — see the JIT roadmap doc.
//!
//! Landing the lifecycle first means future Value-rep changes
//! have a stable trait surface to plug into. The cost today is
//! one allocator construction + drop per HTTP request — negligible
//! compared to the request itself.
//!
//! ## Lifetime model
//!
//! - One arena per request handler invocation.
//! - Arena owns a bump-allocated page chain. `alloc` is a
//!   pointer-bump on the current page; full pages chain into a
//!   `Vec<Page>` and the whole vec drops at scope-exit.
//! - Cannot outlive the request — values built into the arena
//!   must not escape via channels / captures / closures. That's
//!   why the verifier-based escape analysis in #464 is a
//!   prerequisite for actually routing allocations through it;
//!   for now nothing escapes because nothing uses it.

use std::cell::UnsafeCell;

/// Page size in bytes for arena allocations. 64 KiB matches typical
/// L2 line patterns and is large enough that most request-scoped
/// values fit in a single page. Pages are heap-allocated; the chain
/// grows on demand.
const PAGE_BYTES: usize = 64 * 1024;

/// A bump allocator backing one request's allocations.
///
/// `alloc` returns an unaligned byte cursor; callers responsible
/// for alignment. Drop releases all pages at once — no per-value
/// destructor is run, so callers must put only POD-shaped data
/// here today (slice-1 doesn't actually allocate Values into the
/// arena; this is purely a lifecycle harness).
pub struct Arena {
    /// All allocated pages in order. The last page is the active
    /// bump target; previous pages are full.
    pages: UnsafeCell<Vec<Box<[u8; PAGE_BYTES]>>>,
    /// Bump cursor within the active page.
    cursor: UnsafeCell<usize>,
}

impl Arena {
    /// Create an empty arena. No pages allocated until the first
    /// `alloc` call.
    pub fn new() -> Self {
        Self {
            pages: UnsafeCell::new(Vec::new()),
            cursor: UnsafeCell::new(0),
        }
    }

    /// Allocate `len` bytes from the arena. Returns a `&mut [u8]`
    /// bound to the arena's lifetime. Grows by appending a fresh
    /// page if the request doesn't fit in the active page.
    ///
    /// Panics if `len > PAGE_BYTES`. Larger allocations would
    /// require multi-page allocation (boxed slice). Out of scope
    /// for the scaffolding — the IndexMap / VecDeque values the
    /// arena will eventually carry are well under 64 KiB.
    ///
    /// # Safety
    ///
    /// The returned slice is uninitialized.
    ///
    // `mut_from_ref` is the canonical bump-allocator shape (same as
    // `bumpalo::Bump::alloc` and `typed_arena::Arena::alloc`): an
    // immutable `&self` hands out a fresh mutable region per call.
    // Soundness rests on (a) `UnsafeCell` for interior mutability,
    // (b) `!Sync` so no two threads call this at once, and
    // (c) the strict bump invariant — every returned slice starts
    // past every prior slice's end, so references never alias.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc(&self, len: usize) -> &mut [u8] {
        assert!(len <= PAGE_BYTES, "arena alloc exceeds page size");
        // SAFETY: `Arena` is `!Sync` (UnsafeCell), so no concurrent
        // mutation. The reference returned by this call doesn't
        // alias any prior reference because we strictly bump.
        unsafe {
            let pages = &mut *self.pages.get();
            let cursor = &mut *self.cursor.get();
            if pages.is_empty() || *cursor + len > PAGE_BYTES {
                pages.push(Box::new([0u8; PAGE_BYTES]));
                *cursor = 0;
            }
            let page_idx = pages.len() - 1;
            let start = *cursor;
            *cursor += len;
            let page: *mut [u8; PAGE_BYTES] = pages[page_idx].as_mut() as *mut _;
            std::slice::from_raw_parts_mut((*page).as_mut_ptr().add(start), len)
        }
    }

    /// Total bytes allocated across all pages. Useful for tests
    /// and a future `arena.stat` builtin that surfaces per-request
    /// allocation pressure.
    pub fn bytes_allocated(&self) -> usize {
        // SAFETY: see `alloc` — no concurrent mutation.
        unsafe {
            let pages = &*self.pages.get();
            let cursor = *self.cursor.get();
            if pages.is_empty() { 0 } else { (pages.len() - 1) * PAGE_BYTES + cursor }
        }
    }

    /// Page count. Tests use this to confirm grow-on-demand fires
    /// at the right thresholds.
    pub fn page_count(&self) -> usize {
        // SAFETY: see `alloc`.
        unsafe { (*self.pages.get()).len() }
    }
}

impl Default for Arena {
    fn default() -> Self { Self::new() }
}

/// Identifier handed out by `EffectHandler::enter_request_scope`
/// and returned to `EffectHandler::exit_request_scope`. The
/// implementation chooses its representation; the scaffolding's
/// `DefaultHandler` uses a monotonic counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_arena_allocates_no_pages() {
        let a = Arena::new();
        assert_eq!(a.page_count(), 0);
        assert_eq!(a.bytes_allocated(), 0);
    }

    #[test]
    fn first_alloc_creates_a_page() {
        let a = Arena::new();
        let _b = a.alloc(16);
        assert_eq!(a.page_count(), 1);
        assert_eq!(a.bytes_allocated(), 16);
    }

    #[test]
    fn alloc_grows_to_a_new_page_when_full() {
        let a = Arena::new();
        let _b1 = a.alloc(PAGE_BYTES - 100);
        assert_eq!(a.page_count(), 1);
        let _b2 = a.alloc(200);
        assert_eq!(a.page_count(), 2);
    }

    #[test]
    fn alloc_returns_distinct_regions() {
        let a = Arena::new();
        let b1 = a.alloc(8);
        b1[0] = 0xAB;
        let b2 = a.alloc(8);
        b2[0] = 0xCD;
        // No overlap — writes to b2 didn't clobber b1.
        assert_eq!(b1[0], 0xAB);
        assert_eq!(b2[0], 0xCD);
    }

    #[test]
    fn alloc_larger_than_page_panics() {
        let a = Arena::new();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            a.alloc(PAGE_BYTES + 1);
        }));
        assert!(r.is_err());
    }
}
