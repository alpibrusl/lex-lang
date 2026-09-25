//! The import report: what the run did, as JSON (`--output json`, and a local
//! copy under `<store>/import/<branch>.json`) and as text.

use super::{Refusal, Unsupported};
use ::acli::OutputFormat;
use lex_vcs::OpId;
use serde_json::{json, Value};
use std::path::Path;

/// A commit that was skipped and folded into the next importable one.
pub(super) struct Folded {
    pub sha: String,
    pub refusal: Refusal,
    /// The commit whose `origin.folded` carries it (`None` while no later
    /// commit has landed — the run ended, or halted, first).
    pub into: Option<String>,
}

#[derive(Default, Clone, Debug)]
pub(super) struct Stats {
    pub commits: u64,
    pub semantic_passes: u64,
    pub semantic_skipped: u64,
    pub cache_hits: u64,
}

pub(super) struct Report {
    pub source: String,
    pub source_kind: &'static str,
    pub shallow: bool,
    pub head_only: bool,
    pub git_branch: String,
    pub store_branch: String,
    /// The last commit the store branch already had (an incremental run).
    pub watermark: Option<String>,
    pub since: Option<String>,
    /// `{commit, via}` when the lineage started at a `--head-only`/`--since`
    /// snapshot: history before `commit` was never imported.
    pub snapshot_base: Option<Value>,
    pub imported: Vec<Value>,
    pub folded: Vec<Folded>,
    pub noop: Vec<String>,
    pub unsupported: Vec<Unsupported>,
    /// The commit this run aimed to land (the branch tip, or the last commit
    /// `--max-commits` allowed).
    pub tip_sha: String,
    pub requested_tip: String,
    /// First-parent commits left after a `--max-commits` cut.
    pub remaining: usize,
    pub tip_landed: bool,
    /// The refusal that stopped the run, or the tip's own.
    pub failed: Option<(String, Refusal)>,
    pub strict_violation: bool,
    pub head_op: Option<OpId>,
    /// Commits walked this run.
    pub total: usize,
    pub blob_reads: u64,
    pub size_queries: u64,
    pub stats: Stats,
    pub elapsed_ms: u128,
    pub notes: Vec<String>,
}

impl Report {
    pub(super) fn landed(&self) -> bool {
        self.tip_landed && !self.strict_violation
    }

    pub(super) fn to_json(&self) -> Value {
        let unsupported: Vec<Value> = self
            .unsupported
            .iter()
            .map(|u| {
                json!({
                    "path": u.path,
                    "kind": u.kind,
                    // `commit` is where it was first seen (a persistent symlink
                    // is listed once, not once per commit).
                    "commit": u.first_seen,
                    "first_seen": u.first_seen,
                    "last_seen": u.last_seen,
                })
            })
            .collect();
        let folded: Vec<Value> = self
            .folded
            .iter()
            .map(|f| {
                json!({
                    "sha": f.sha,
                    "phase": f.refusal.phase,
                    "reason": f.refusal.reason,
                    "message": f.refusal.message,
                    "diagnostics": f.refusal.diagnostics,
                    "folded_into": f.into,
                })
            })
            .collect();
        let mut tip = json!({ "sha": self.tip_sha, "landed": self.landed() });
        if let Some((sha, f)) = &self.failed {
            tip["failed_commit"] = json!(sha);
            tip["phase"] = json!(f.phase);
            tip["reason"] = json!(f.reason);
            tip["message"] = json!(f.message);
            tip["diagnostics"] = json!(f.diagnostics);
        }
        let n_folded = self.folded.len();
        let ratio = if self.total == 0 { 0.0 } else { n_folded as f64 / self.total as f64 };
        json!({
            "source": { "kind": self.source_kind, "display": self.source },
            "shallow": self.shallow,
            "mode": if self.head_only { "head-only" } else { "history" },
            "git_branch": self.git_branch,
            "store_branch": self.store_branch,
            "watermark": self.watermark,
            "since": self.since,
            "snapshot_base": self.snapshot_base,
            "imported": self.imported,
            "folded": folded,
            "noop": self.noop,
            "unsupported": unsupported,
            "tip": tip,
            "requested_tip": self.requested_tip,
            "truncated": self.remaining > 0,
            "remaining": self.remaining,
            "head_op": self.head_op,
            "stats": {
                "commits": self.total,
                "imported": self.imported.len(),
                "folded": n_folded,
                "noop": self.noop.len(),
                "fold_ratio": ratio,
                "semantic_passes": self.stats.semantic_passes,
                "semantic_skipped": self.stats.semantic_skipped,
                "blob_reads": self.blob_reads,
                "blob_cache_hits": self.stats.cache_hits,
                "size_queries": self.size_queries,
                "elapsed_ms": self.elapsed_ms,
            },
            "notes": self.notes,
            "toolchain": format!("lex {}", crate::acli::VERSION),
        })
    }

    /// A local convenience copy, never a source of truth and never synced.
    pub(super) fn write_file(&self, store_root: &Path) {
        let dir = store_root.join("import");
        let path = dir.join(format!("{}.json", self.store_branch));
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".{}.json.tmp", self.store_branch));
            std::fs::write(&tmp, serde_json::to_string_pretty(&self.to_json())?)?;
            std::fs::rename(tmp, &path)
        };
        if let Err(e) = write() {
            eprintln!("warning: could not write the import report to {}: {e}", path.display());
        }
    }
}

pub(super) fn render(fmt: &OutputFormat, r: &Report) {
    let text = || {
        let n_folded = r.folded.len();
        if r.head_only && r.failed.is_none() && r.landed() {
            match r.imported.first() {
                None => println!("tip {} changed nothing; nothing imported", r.tip_sha),
                Some(i) => {
                    println!(
                        "imported tip {} onto store branch `{}` ({} op(s){})",
                        r.tip_sha,
                        r.store_branch,
                        i["ops"],
                        match i["files_op"].as_str() {
                            Some(f) => format!(", files op {f}"),
                            None => String::new(),
                        }
                    );
                    if let Some(h) = &r.head_op {
                        println!("head: {h}");
                    }
                }
            }
        } else {
            let ops: u64 = r.imported.iter().map(|i| i["ops"].as_u64().unwrap_or(0)).sum();
            if r.total == 0 {
                println!(
                    "store branch `{}` is up to date with {}; nothing to import",
                    r.store_branch, r.tip_sha
                );
            } else {
                println!(
                    "walked {} commit(s) onto store branch `{}`: {} imported ({} op(s)), {} folded, {} with no net change",
                    r.total,
                    r.store_branch,
                    r.imported.len(),
                    ops,
                    n_folded,
                    r.noop.len()
                );
            }
            if let Some(h) = &r.head_op {
                println!("head: {h}");
            }
            if r.remaining > 0 {
                println!("stopped by --max-commits: {} commit(s) remain before {}", r.remaining, r.requested_tip);
            }
            match &r.failed {
                Some((sha, f)) => {
                    println!("tip {} did NOT land ({} failed: {}: {})", r.tip_sha, sha, f.phase, f.message);
                    for d in &f.diagnostics {
                        eprintln!("{d}");
                    }
                }
                None if !r.landed() => println!("tip {} did NOT land", r.tip_sha),
                None => println!("tip {} landed", r.tip_sha),
            }
        }
        for f in &r.folded {
            println!("folded {} ({}: {})", f.sha, f.refusal.phase, f.refusal.message);
        }
        for u in &r.unsupported {
            println!(
                "unsupported {}: {} (first seen {}, last seen {})",
                u.kind, u.path, u.first_seen, u.last_seen
            );
        }
        for n in &r.notes {
            println!("note: {n}");
        }
    };
    crate::acli::emit_or_text("op-import-git", r.to_json(), fmt, text);
}
