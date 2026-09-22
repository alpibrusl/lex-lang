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
use lex_store::{StageStatus, Store};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One indexed stage record. The three optional embedding fields
/// carry the per-component vectors; ranking pulls them out to score
/// against a query embedding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexedStage {
    pub stage_id: String,
    pub sig_id: String,
    pub name: String,
    /// Lifecycle status of the indexed stage: `Active`, or `Draft` when the
    /// index was built with [`BuildOptions::include_draft`] (#969).
    pub status: StageStatus,
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
    /// Lifecycle status of the hit, so a Draft result is never mistaken for
    /// an Active one (#969).
    pub status: StageStatus,
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

/// What [`SearchIndex::build_with`] indexes.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Also index functions that have **no** Active stage, using a Draft one.
    ///
    /// Lifecycle status is local to a store — it is not an op and is not
    /// carried by `op push`/`op pull` — so every stage a store *received*
    /// rather than authored is Draft (#969). Without this, a pulled store is
    /// invisible to search.
    pub include_draft: bool,
    /// `SigId -> StageId` of a branch head. When a function has several
    /// Draft stages (one per version), the one the head names is the one
    /// indexed; otherwise the most recently transitioned Draft is used.
    pub prefer: BTreeMap<String, String>,
}

/// Walk the searchable stages of `store`, render each into the three
/// search-relevant strings, and embed everything in batch.
///
/// Per function (SigId) the Active stage is indexed. A function with no
/// Active stage but at least one Draft is indexed from a Draft only under
/// [`BuildOptions::include_draft`]; otherwise it is counted in
/// [`SearchIndex::drafts_skipped`] so callers can say *why* a search came
/// back empty instead of reporting "no matches". Deprecated and tombstoned
/// stages are never indexed.
pub struct SearchIndex {
    pub stages: Vec<IndexedStage>,
    /// Functions left out because their only candidate stage is a Draft
    /// (always 0 under [`BuildOptions::include_draft`]).
    pub drafts_skipped: usize,
}

impl SearchIndex {
    /// Index Active stages only — [`Self::build_with`] with default options.
    pub fn build(store: &Store, embedder: &dyn Embedder) -> Result<Self, BuildError> {
        Self::build_with(store, embedder, &BuildOptions::default())
    }

    pub fn build_with(
        store: &Store,
        embedder: &dyn Embedder,
        opts: &BuildOptions,
    ) -> Result<Self, BuildError> {
        let mut staging: Vec<StagingRow> = Vec::new();
        let mut drafts_skipped = 0usize;
        for sig in store.list_sigs()? {
            // Newest-first, one entry per stage with its latest status.
            let history = store.sig_history(&sig)?;
            let (chosen, status) = match history.iter().find(|e| e.status == StageStatus::Active) {
                Some(e) => (e.stage_id.clone(), StageStatus::Active),
                None => {
                    let drafts: Vec<&str> = history
                        .iter()
                        .filter(|e| e.status == StageStatus::Draft)
                        .map(|e| e.stage_id.as_str())
                        .collect();
                    let Some(newest) = drafts.first() else {
                        continue;
                    };
                    let pick = opts
                        .prefer
                        .get(&sig)
                        .map(String::as_str)
                        .filter(|p| drafts.contains(p))
                        .unwrap_or(newest);
                    (pick.to_string(), StageStatus::Draft)
                }
            };
            // Read through the SigId we already hold: a StageId is
            // name-independent and may be filed under several sigs (#826).
            let meta = match store.get_metadata_for_sig(&sig, &chosen) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let pair = [(sig.clone(), chosen.clone())];
            let ast = match store.get_asts_for_sigs_bulk(&pair).pop() {
                Some(Ok(s)) => s,
                _ => continue,
            };
            let fd = match &ast {
                Stage::FnDecl(fd) => fd,
                _ => continue,
            };
            if status == StageStatus::Draft && !opts.include_draft {
                drafts_skipped += 1;
                continue;
            }
            let signature = render_signature(fd);
            let description = meta.note.clone().filter(|s| !s.is_empty());
            let examples = collect_examples(store, &sig);
            staging.push(StagingRow {
                stage_id: meta.stage_id.clone(),
                sig_id: meta.sig_id.clone(),
                name: meta.name.clone(),
                status,
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
            status: row.status,
            signature: row.signature,
            description: row.description,
            examples: row.examples.clone(),
            description_emb: None,
            signature_emb: vec![0.0; dim],
            example_embs: vec![vec![0.0; dim]; row.examples.len()],
        }).collect();

        for (kind, vec) in kinds.into_iter().zip(embeddings) {
            let row = &mut indexed[kind.row];
            match kind.slot {
                Slot::Description => row.description_emb = Some(vec),
                Slot::Signature => row.signature_emb = vec,
                Slot::Example(j) => row.example_embs[j] = vec,
            }
        }

        Ok(Self { stages: indexed, drafts_skipped })
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
                status: s.status,
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
    status: StageStatus,
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
        TypeExpr::RecordWithSpreads { spreads, fields } => {
            let mut parts: Vec<String> = spreads.iter().map(|s| format!("...{}", s)).collect();
            parts.extend(fields.iter().map(|f| format!("{} :: {}", f.name, render_type(&f.ty))));
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
