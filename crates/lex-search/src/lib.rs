//! Semantic search over a `lex-store` (#224).
//!
//! Agents need to discover stages by *intent*, not by exact name.
//! `lex audit` already covers structural search (effect filter,
//! call-site filter, host substring); this crate adds an embedding-
//! based ranker on top of three independent indexes:
//!
//!   - **description** (~0.5) — the stage's `Metadata.note`, when
//!     present. Closest thing we have to a doc comment until comments
//!     survive into canonical AST.
//!   - **signature** (~0.3) — `name(params) -> ret [effects]` rendered
//!     as a single string. Cheap to embed and surprisingly informative
//!     because Lex types are descriptive (`File`, `Url`, `Customer`).
//!   - **examples** (~0.2) — `input => output` pairs, max-pooled per
//!     stage. Anchors the embedding around the actual behaviour.
//!
//! Fusion is a simple weighted sum of cosine similarities. With an
//! embedding model that puts similar concepts close in vector space,
//! a query like "parse CSV and return rows as records" matches a
//! `parse_csv :: String -> List[Record]` even if the literal token
//! "csv" never appears in the description.
//!
//! ## What's in vs out for slice 1
//!
//! In:
//!   - [`Embedder`] trait + [`MockEmbedder`] (deterministic hash-
//!     based); enough to give CLI tests semantic ordering without
//!     calling out to a real model.
//!   - [`SearchIndex`] over a `lex_store::Store`.
//!   - [`fuse_scores`] / [`cosine_similarity`] as standalone
//!     primitives so the CLI can format breakdowns.
//!
//! Deferred:
//!   - HTTP embedder backends (Ollama, OpenAI-compat) — Slice 2,
//!     gated on `LEX_EMBED_URL` env var.
//!   - On-disk index cache to avoid re-embedding on every query
//!     once a real backend is wired.
//!   - HNSW for stores past ~500 stages. Brute force is sub-ms
//!     well past that.

use serde::{Deserialize, Serialize};

mod cache;
mod embedder;
mod http;
mod index;
mod mock;
mod scoring;

pub use cache::{default_cache_root, CachingEmbedder};
pub use embedder::{EmbedError, Embedder};
pub use http::{HttpEmbedder, Provider};
pub use index::{IndexedStage, SearchHit, SearchIndex};
pub use mock::MockEmbedder;
pub use scoring::{cosine_similarity, fuse_scores};

/// Default fusion weights from the issue brief. Description weighs
/// most because doc text is the highest-fidelity signal of intent;
/// examples weigh least because not every stage has them.
///
/// Renormalisation rule: when a stage has no description, the 0.5
/// weight is folded into the signature (signature is always present).
/// The examples weight stays put when examples are missing — its
/// score just becomes 0 for that stage. This is intentional: a stage
/// without examples shouldn't be penalised relative to one with
/// examples that don't match the query.
pub const W_DESCRIPTION: f32 = 0.5;
pub const W_SIGNATURE: f32 = 0.3;
pub const W_EXAMPLES: f32 = 0.2;

/// Per-component breakdown attached to a [`SearchHit`]. Surfaced in
/// `--explain` mode so users / agents can debug "why did this rank
/// where it did".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoreBreakdown {
    pub description: Option<f32>,
    pub signature: f32,
    pub examples: Option<f32>,
    pub fused: f32,
}
