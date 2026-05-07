//! Build an in-memory three-index over the active stages of a
//! `lex_store::Store`, then rank candidates against a query.
//!
//! The index materialises three vectors per stage (description,
//! signature, examples). At query time we embed the query once and
//! compute a fused score against every stage. Brute force is fine
//! up to a few hundred stages — see issue notes for the HNSW
//! follow-up plan.

use crate::embedder::{EmbedError, Embedder};
use crate::scoring::{cosine_similarity, fuse_scores};
use crate::ScoreBreakdown;
use lex_ast::{FnDecl, Stage, TypeExpr};
use lex_store::Store;
use serde::{Deserialize, Serialize};

/// One indexed stage record. The three optional embedding fields
/// carry the per-component vectors; ranking pulls them out to score
/// against a query embedding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexedStage {
    pub stage_id: String,
    pub sig_id: String,
    pub name: String,
    /// Rendered `name(params) -> ret [effects]`. Always present.
    pub signature: String,
    /// Free-form note attached to the stage's metadata, when any.
    pub description: Option<String>,
    /// `input => output` strings, one per attached test. Empty when
    /// the stage has no examples.
    pub examples: Vec<String>,
    /// L2-normalised description embedding, when [`Self::description`]
    /// is set.
    pub description_emb: Option<Vec<f32>>,
    /// Signature embedding (always present).
    pub signature_emb: Vec<f32>,
    /// Per-example embeddings; max-pooled at query time.
    pub example_embs: Vec<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub stage_id: String,
    pub sig_id: String,
    pub name: String,
    pub signature: String,
    pub description: Option<String>,
    pub score: ScoreBreakdown,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("store error: {0}")]
    Store(#[from] lex_store::StoreError),
    #[error("embed error: {0}")]
    Embed(#[from] EmbedError),
}

/// Walk the active stages of `store`, render each into the three
/// search-relevant strings, and embed everything in batch.
///
/// Only `Active` stages are indexed: drafts and deprecated stages
/// would inflate the result list with stale candidates. If a SigId
/// has no active stage (all drafts) it's skipped.
pub struct SearchIndex {
    pub stages: Vec<IndexedStage>,
}

impl SearchIndex {
    pub fn build(store: &Store, embedder: &dyn Embedder) -> Result<Self, BuildError> {
        let mut staging: Vec<StagingRow> = Vec::new();
        for sig in store.list_sigs()? {
            let active = match store.resolve_sig(&sig)? {
                Some(stage_id) => stage_id,
                None => continue,
            };
            let meta = match store.get_metadata(&active) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ast = match store.get_ast(&active) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let fd = match &ast {
                Stage::FnDecl(fd) => fd,
                _ => continue,
            };
            let signature = render_signature(fd);
            let description = meta.note.clone().filter(|s| !s.is_empty());
            let examples = collect_examples(store, &sig);
            staging.push(StagingRow {
                stage_id: meta.stage_id.clone(),
                sig_id: meta.sig_id.clone(),
                name: meta.name.clone(),
                signature,
                description,
                examples,
            });
        }

        // One big batch keeps round-trips down for HTTP embedders.
        // `text_kinds` lets us slice the result back into per-stage
        // structs after embedding finishes.
        let mut texts: Vec<&str> = Vec::new();
        let mut kinds: Vec<TextKind> = Vec::new();
        for (i, row) in staging.iter().enumerate() {
            if let Some(d) = &row.description {
                texts.push(d.as_str());
                kinds.push(TextKind { row: i, slot: Slot::Description });
            }
            texts.push(&row.signature);
            kinds.push(TextKind { row: i, slot: Slot::Signature });
            for (j, ex) in row.examples.iter().enumerate() {
                texts.push(ex.as_str());
                kinds.push(TextKind { row: i, slot: Slot::Example(j) });
            }
        }

        let embeddings = embedder.embed_batch(&texts)?;
        let dim = embedder.dim();

        let mut indexed: Vec<IndexedStage> = staging.into_iter().map(|row| IndexedStage {
            stage_id: row.stage_id,
            sig_id: row.sig_id,
            name: row.name,
            signature: row.signature,
            description: row.description,
            examples: row.examples.clone(),
            description_emb: None,
            signature_emb: vec![0.0; dim],
            example_embs: vec![vec![0.0; dim]; row.examples.len()],
        }).collect();

        for (kind, vec) in kinds.into_iter().zip(embeddings.into_iter()) {
            let row = &mut indexed[kind.row];
            match kind.slot {
                Slot::Description => row.description_emb = Some(vec),
                Slot::Signature => row.signature_emb = vec,
                Slot::Example(j) => row.example_embs[j] = vec,
            }
        }

        Ok(Self { stages: indexed })
    }

    /// Rank every indexed stage against `query`. Returns the top
    /// `limit` hits sorted by fused score, descending. Ties break
    /// on `name` so output is deterministic across runs.
    pub fn query(
        &self,
        embedder: &dyn Embedder,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, EmbedError> {
        if limit == 0 || self.stages.is_empty() {
            return Ok(Vec::new());
        }
        let q = embedder.embed(query)?;
        let mut hits: Vec<SearchHit> = self.stages.iter()
            .map(|s| SearchHit {
                stage_id: s.stage_id.clone(),
                sig_id: s.sig_id.clone(),
                name: s.name.clone(),
                signature: s.signature.clone(),
                description: s.description.clone(),
                score: score_stage(&q, s),
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score.fused.partial_cmp(&a.score.fused)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

fn score_stage(q: &[f32], s: &IndexedStage) -> ScoreBreakdown {
    let desc = s.description_emb.as_ref().map(|e| cosine_similarity(q, e));
    let sig = cosine_similarity(q, &s.signature_emb);
    let ex = if s.example_embs.is_empty() {
        None
    } else {
        // Max-pool: a single excellent example anchors the stage.
        let scores = s.example_embs.iter()
            .map(|e| cosine_similarity(q, e));
        scores.fold(None, |acc, x| Some(match acc {
            Some(prev) if prev >= x => prev,
            _ => x,
        }))
    };
    fuse_scores(desc, sig, ex)
}

struct StagingRow {
    stage_id: String,
    sig_id: String,
    name: String,
    signature: String,
    description: Option<String>,
    examples: Vec<String>,
}

struct TextKind {
    row: usize,
    slot: Slot,
}
enum Slot { Description, Signature, Example(usize) }

fn collect_examples(store: &Store, sig: &str) -> Vec<String> {
    let tests = match store.list_tests(sig) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    tests.into_iter().map(|t| {
        let input = serde_json::to_string(&t.input).unwrap_or_default();
        let output = serde_json::to_string(&t.expected_output).unwrap_or_default();
        format!("{input} => {output}")
    }).collect()
}

fn render_signature(fd: &FnDecl) -> String {
    let params: Vec<String> = fd.params.iter()
        .map(|p| format!("{} :: {}", p.name, render_type(&p.ty)))
        .collect();
    let eff = if fd.effects.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = fd.effects.iter().map(|e| e.name.as_str()).collect();
        format!(" [{}]", names.join(", "))
    };
    format!("{}({}) -> {}{}",
        fd.name, params.join(", "), render_type(&fd.return_type), eff)
}

fn render_type(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named { name, args } => {
            if args.is_empty() { name.clone() }
            else {
                let parts: Vec<String> = args.iter().map(render_type).collect();
                format!("{name}[{}]", parts.join(", "))
            }
        }
        TypeExpr::Tuple { items } => {
            let parts: Vec<String> = items.iter().map(render_type).collect();
            format!("({})", parts.join(", "))
        }
        TypeExpr::Record { fields } => {
            let parts: Vec<String> = fields.iter()
                .map(|f| format!("{} :: {}", f.name, render_type(&f.ty))).collect();
            format!("{{{}}}", parts.join(", "))
        }
        TypeExpr::Function { params, ret, .. } => {
            let parts: Vec<String> = params.iter().map(render_type).collect();
            format!("({}) -> {}", parts.join(", "), render_type(ret))
        }
        TypeExpr::Union { variants } => {
            let parts: Vec<String> = variants.iter()
                .map(|v| v.name.clone()).collect();
            format!("[{}]", parts.join(" | "))
        }
        TypeExpr::Refined { base, .. } => render_type(base),
    }
}
