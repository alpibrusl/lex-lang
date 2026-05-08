//! `lex-tea` v2 — read-only HTML browser over lex-vcs, oriented at
//! the things humans uniquely need (audit, trust calibration,
//! exception triage). The JSON API at `/v1/*` stays the
//! agent-driven surface; this is the human-only complement.
//!
//! Pages:
//!   GET /                  — activity stream (recent attestations)
//!   GET /web/branches      — branch list + head_op
//!   GET /web/trust         — per-producer rollup (pass rate, failures)
//!   GET /web/attention     — exceptions queue (Failed/Inconclusive,
//!                            stale merge sessions)
//!   GET /web/branch/<name> — fns on a branch (detail)
//!   GET /web/stage/<id>    — stage info + attestation trail (detail)
//!
//! Why this shape: agents drive the JSON API in batches. Humans
//! glance at a page once a day to decide whether to intervene.
//! Per-conflict click-resolve UI would be a regression — agents
//! handle that. The home page is the *macro* view; v1's
//! "browse-by-branch" home moves to `/web/branches` as a detail
//! surface.
//!
//! No JS, single embedded CSS blob. Promote to its own crate when
//! it outgrows the single-file footprint.

use lex_store::Store;
use lex_vcs::{AttestationKind, AttestationResult};
use std::collections::BTreeMap;
use std::io::Cursor;
use tiny_http::{Header, Response};

use crate::handlers::State;

const STYLE: &str = r#"
* { box-sizing: border-box; }
body { font: 14px/1.5 -apple-system, system-ui, sans-serif;
       max-width: 920px; margin: 0 auto; padding: 0 1rem; color: #222; }
header { padding: 1.5rem 0 .5rem; border-bottom: 1px solid #eee; margin-bottom: 1rem; }
header .nav { font-size: 13px; }
header .nav a { margin-right: .8rem; color: #666; }
header .nav a.current { color: #0a5; font-weight: 500; }
h1 { font-weight: 600; margin: 0 0 .3rem; font-size: 22px; }
h2 { font-weight: 500; margin: 1.5rem 0 .5rem; color: #444; }
h3 { font-weight: 500; margin: 1rem 0 .3rem; color: #555; font-size: 14px; }
a { color: #0a5; text-decoration: none; }
a:hover { text-decoration: underline; }
nav.crumb { font-size: 13px; color: #888; margin: .5rem 0 1rem; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { text-align: left; padding: .35rem .6rem; border-bottom: 1px solid #eee; vertical-align: top; }
th { color: #666; font-weight: 500; background: #fafafa; }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
.muted { color: #999; }
.tag { display: inline-block; padding: 1px 6px; font-size: 11px; border-radius: 3px;
       background: #eef; color: #335; margin-right: 4px; }
.tag.ok { background: #dfd; color: #060; }
.tag.fail { background: #fdd; color: #800; }
.tag.inc { background: #ffd; color: #850; }
.tag.kind { background: #eef0f3; color: #445; }
.tag.blocked { background: #444; color: #fff; }
.empty { color: #aaa; font-style: italic; padding: 1rem; }
.summary { display: flex; gap: 1.5rem; margin: 1rem 0; }
.stat { padding: .5rem 1rem; background: #fafafa; border-radius: 4px; min-width: 110px; }
.stat .n { font-size: 22px; font-weight: 600; color: #222; line-height: 1.1; }
.stat .l { font-size: 11px; color: #888; text-transform: uppercase; letter-spacing: .04em; }
.stat.fail .n { color: #b00; }
.stat.inc  .n { color: #850; }
.note { background: #f8f8fa; padding: .8rem 1rem; border-left: 3px solid #ddd; color: #666;
        font-size: 13px; margin: 1rem 0; }
"#;

fn html_response(status: u16, body: String) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.into_bytes())
        .with_status_code(status)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
        )
}

/// Top-level page chrome. `current` selects which top-nav link is
/// highlighted (one of "/", "/web/branches", "/web/trust",
/// "/web/attention"); for detail pages pass `""`.
fn page(title: &str, current: &str, body: &str) -> String {
    let nav_link = |href: &str, label: &str| -> String {
        let class = if current == href { "current" } else { "" };
        format!(r#"<a href="{href}" class="{class}">{label}</a>"#)
    };
    let nav = format!(
        "{} {} {} {}",
        nav_link("/", "activity"),
        nav_link("/web/attention", "attention"),
        nav_link("/web/trust", "trust"),
        nav_link("/web/branches", "branches"),
    );
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title} — lex-tea</title>
<style>{STYLE}</style>
</head>
<body>
<header><div class="nav">{nav}</div></header>
{body}
</body>
</html>
"#,
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---- Activity stream (`GET /`) -----------------------------------

/// Walks every attestation in the store, sorts newest-first, and
/// renders one row per event. Why attestations and not ops:
/// attestations have explicit timestamps; ops are
/// content-addressed but the OpRecord has no `recorded_at` field
/// (intentional — preserving op identity). Every accepted op
/// auto-emits a TypeCheck attestation, so the attestation feed is
/// effectively the op feed plus richer signals (Spec, Examples,
/// EffectAudit, SandboxRun).
pub(crate) fn activity_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return html_response(500, page("error", "/", &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    let mut atts = log.list_all().unwrap_or_default();
    atts.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
    // #181: producers on the local policy block list keep their
    // attestations in the log (audit trail intact) but every row
    // gets a `blocked` tag in the feed so reviewers can filter
    // them out by eye.
    let policy = lex_store::policy::load(&state.root)
        .ok().flatten().unwrap_or_default();

    // Headline counters: pass / fail / inconclusive in the last 24h.
    // 24h from "now" via wall clock; small enough to be glanceable,
    // big enough to catch overnight drift.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now.saturating_sub(24 * 60 * 60);
    let recent: Vec<&_> = atts.iter().filter(|a| a.timestamp >= cutoff).collect();
    let pass = recent.iter().filter(|a| matches!(a.result, AttestationResult::Passed)).count();
    let fail = recent.iter().filter(|a| matches!(a.result, AttestationResult::Failed { .. })).count();
    let inc  = recent.iter().filter(|a| matches!(a.result, AttestationResult::Inconclusive { .. })).count();

    let mut rows = String::new();
    if atts.is_empty() {
        rows.push_str(r#"<tr><td colspan="4" class="empty">no activity yet — try `lex publish &lt;file.lex&gt;`</td></tr>"#);
    } else {
        // Cap at 200 rows so a long-lived store doesn't render a
        // page that takes forever to scroll. Older events are
        // reachable via per-stage detail pages.
        for a in atts.iter().take(200) {
            let kind_tag = kind_short(&a.kind);
            let (result_lbl, css) = result_short(&a.result);
            let summary = match &a.result {
                AttestationResult::Failed { detail }
                | AttestationResult::Inconclusive { detail } => esc(detail),
                AttestationResult::Passed => esc(&a.produced_by.tool),
            };
            let blocked_tag = if policy.is_blocked(&a.produced_by.tool) {
                r#" <span class="tag blocked" title="producer is on the policy block list">blocked</span>"#
            } else {
                ""
            };
            rows.push_str(&format!(
                r#"<tr>
                  <td class="muted mono">{ts}</td>
                  <td><span class="tag kind">{kind}</span>{blocked_tag}</td>
                  <td><span class="tag {css}">{result}</span> <span class="muted">{summary}</span></td>
                  <td class="mono"><a href="/web/stage/{stage}">{stage:.16}…</a></td>
                </tr>"#,
                ts = a.timestamp,
                kind = esc(&kind_tag),
                css = css,
                result = result_lbl,
                summary = summary,
                stage = esc(&a.stage_id),
            ));
        }
    }

    let body = format!(
        r#"<h1>activity</h1>
<p class="muted">Reverse-chrono feed of attestations — one event per row.
Every accepted op auto-emits a TypeCheck; Spec / Examples / DiffBody /
SandboxRun / EffectAudit show up as agents and CI run them.</p>
<div class="summary">
  <div class="stat"><div class="n">{pass}</div><div class="l">passed (24h)</div></div>
  <div class="stat fail"><div class="n">{fail}</div><div class="l">failed (24h)</div></div>
  <div class="stat inc"><div class="n">{inc}</div><div class="l">inconclusive (24h)</div></div>
  <div class="stat"><div class="n">{total}</div><div class="l">total events</div></div>
</div>
<table>
  <thead><tr><th>ts</th><th>kind</th><th>result</th><th>stage</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
        total = atts.len(),
    );
    html_response(200, page("activity", "/", &body))
}

// ---- Trust (`GET /web/trust`) ------------------------------------

/// Per-producer rollup: count by result, recent-failure latch.
/// Designed to surface *which* model / tool is regressing without
/// asking the human to scan rows.
pub(crate) fn trust_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return html_response(500, page("error", "/web/trust",
            &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    let atts = log.list_all().unwrap_or_default();

    // Group by (tool, model). The `model` is `None` for
    // store-side / harness-side producers; `Some(name)` when an
    // LLM was the proximate producer (e.g. `lex agent-tool`
    // with `--request`). We keep them separated so a model
    // regression doesn't get hidden in the tool's totals.
    #[derive(Default)]
    struct Stats {
        passed: usize,
        failed: usize,
        inconclusive: usize,
        latest_failure_ts: Option<u64>,
        latest_failure_stage: Option<String>,
    }
    let mut groups: BTreeMap<(String, String), Stats> = BTreeMap::new();
    for a in &atts {
        let key = (
            a.produced_by.tool.clone(),
            a.produced_by.model.clone().unwrap_or_else(|| "—".into()),
        );
        let s = groups.entry(key).or_default();
        match &a.result {
            AttestationResult::Passed => s.passed += 1,
            AttestationResult::Failed { .. } => {
                s.failed += 1;
                if s.latest_failure_ts.map(|t| t < a.timestamp).unwrap_or(true) {
                    s.latest_failure_ts = Some(a.timestamp);
                    s.latest_failure_stage = Some(a.stage_id.clone());
                }
            }
            AttestationResult::Inconclusive { .. } => s.inconclusive += 1,
        }
    }

    let mut rows = String::new();
    if groups.is_empty() {
        rows.push_str(r#"<tr><td colspan="6" class="empty">no producers have attested yet</td></tr>"#);
    } else {
        // Sort by recent-failure first (descending ts), then by
        // total volume. The thing humans want at the top of the
        // page is "who broke recently."
        let mut entries: Vec<_> = groups.into_iter().collect();
        entries.sort_by(|a, b| {
            b.1.latest_failure_ts.cmp(&a.1.latest_failure_ts)
                .then_with(|| (b.1.passed + b.1.failed + b.1.inconclusive)
                    .cmp(&(a.1.passed + a.1.failed + a.1.inconclusive)))
        });
        for ((tool, model), s) in &entries {
            let total = s.passed + s.failed + s.inconclusive;
            let pct = (s.passed * 100).checked_div(total).unwrap_or(0);
            let recent_fail = match (&s.latest_failure_ts, &s.latest_failure_stage) {
                (Some(ts), Some(stage)) => format!(
                    r#"<a href="/web/stage/{stage}" class="mono">{stage:.16}…</a> <span class="muted">@{ts}</span>"#,
                    stage = esc(stage), ts = ts,
                ),
                _ => r#"<span class="muted">—</span>"#.into(),
            };
            rows.push_str(&format!(
                r#"<tr>
                  <td>{tool}</td>
                  <td class="mono">{model}</td>
                  <td>{total}</td>
                  <td>{pass} <span class="muted">({pct}%)</span></td>
                  <td>{fail}</td>
                  <td>{recent_fail}</td>
                </tr>"#,
                tool = esc(tool),
                model = esc(model),
                total = total,
                pass = s.passed,
                pct = pct,
                fail = s.failed,
                recent_fail = recent_fail,
            ));
        }
    }

    let body = format!(
        r#"<h1>trust</h1>
<p class="muted">One row per producer (<code>tool</code>, <code>model</code>). Sorted
by most-recent failure first so a regressing model rises to the top.
Pass rate is over the producer's lifetime; the
<strong>latest failure</strong> column links to the stage so you can
read the detail.</p>
<table>
  <thead><tr>
    <th>tool</th><th>model</th><th>total</th><th>passed</th>
    <th>failed</th><th>latest failure</th>
  </tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
    );
    html_response(200, page("trust", "/web/trust", &body))
}

// ---- Attention (`GET /web/attention`) ----------------------------

/// The "pay attention to this" list: failed / inconclusive
/// attestations from the last week, plus open merge sessions.
/// What's left here is what humans should look at.
pub(crate) fn attention_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return html_response(500, page("error", "/web/attention",
            &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    let mut atts = log.list_all().unwrap_or_default();
    atts.sort_by_key(|a| std::cmp::Reverse(a.timestamp));

    // 7-day window for the exceptions table — long enough that an
    // overnight regression is still visible after a weekend; short
    // enough that the page doesn't grow unbounded.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now.saturating_sub(7 * 24 * 60 * 60);

    let exceptions: Vec<&_> = atts.iter()
        .filter(|a| a.timestamp >= cutoff)
        .filter(|a| !matches!(a.result, AttestationResult::Passed))
        .collect();

    let mut ex_rows = String::new();
    if exceptions.is_empty() {
        ex_rows.push_str(r#"<tr><td colspan="4" class="empty">no failed or inconclusive attestations in the last 7 days — clear runway</td></tr>"#);
    } else {
        for a in &exceptions {
            let kind = kind_short(&a.kind);
            let (lbl, css) = result_short(&a.result);
            let detail = match &a.result {
                AttestationResult::Failed { detail }
                | AttestationResult::Inconclusive { detail } => esc(detail),
                _ => String::new(),
            };
            ex_rows.push_str(&format!(
                r#"<tr>
                  <td class="muted mono">{ts}</td>
                  <td><span class="tag kind">{kind}</span> <span class="tag {css}">{lbl}</span></td>
                  <td>{detail}</td>
                  <td class="mono"><a href="/web/stage/{stage}">{stage:.16}…</a></td>
                </tr>"#,
                ts = a.timestamp,
                kind = esc(&kind),
                css = css,
                lbl = lbl,
                detail = detail,
                stage = esc(&a.stage_id),
            ));
        }
    }

    // Open merge sessions: read the directory directly. Each
    // session file is a JSON envelope persisted by `lex merge
    // start`. Show its src/dst branches and how stale it is so a
    // reviewer can spot a session the agent forgot to commit.
    let merges_dir = store.root().join("merges");
    let mut merge_rows = String::new();
    let mut merge_count = 0usize;
    if let Ok(entries) = std::fs::read_dir(&merges_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_none_or(|e| e != "json") { continue; }
            let bytes = match std::fs::read(&p) { Ok(b) => b, Err(_) => continue };
            let v: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v, Err(_) => continue,
            };
            let id = p.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
            let src = v["src_branch"].as_str().unwrap_or("?").to_string();
            let dst = v["dst_branch"].as_str().unwrap_or("?").to_string();
            let conflicts = v.pointer("/session/conflicts")
                .and_then(|c| c.as_object())
                .map(|o| o.len())
                .unwrap_or(0);
            let mtime = entry.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let age_secs = now.saturating_sub(mtime);
            let age = if age_secs < 3600 { format!("{}m", age_secs / 60) }
                else if age_secs < 86_400 { format!("{}h", age_secs / 3600) }
                else { format!("{}d", age_secs / 86_400) };
            merge_rows.push_str(&format!(
                r#"<tr>
                  <td class="mono">{id}</td>
                  <td>{src} → {dst}</td>
                  <td>{conflicts}</td>
                  <td class="muted">{age} old</td>
                </tr>"#,
                id = esc(&id),
                src = esc(&src),
                dst = esc(&dst),
                conflicts = conflicts,
                age = age,
            ));
            merge_count += 1;
        }
    }
    if merge_count == 0 {
        merge_rows = r#"<tr><td colspan="4" class="empty">no in-flight merges</td></tr>"#.into();
    }

    let body = format!(
        r#"<h1>attention</h1>
<p class="muted">The "pay attention to this" list — exceptions from
the last 7 days, plus in-flight merge sessions. If both tables show
"empty," there's nothing for a reviewer to do.</p>

<h2>exceptions</h2>
<table>
  <thead><tr><th>ts</th><th>kind</th><th>detail</th><th>stage</th></tr></thead>
  <tbody>{ex_rows}</tbody>
</table>

<h2>open merge sessions</h2>
<table>
  <thead><tr><th>merge_id</th><th>branches</th><th>conflicts</th><th>age</th></tr></thead>
  <tbody>{merge_rows}</tbody>
</table>"#,
    );
    html_response(200, page("attention", "/web/attention", &body))
}

// ---- Branch list (`GET /web/branches`) ----------------------------

/// Moved from v1's `/`. Still useful as a detail page; the new
/// home is the activity stream.
pub(crate) fn branches_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let branches = match store.list_branches() {
        Ok(b) => b,
        Err(e) => return html_response(500, page("error", "/web/branches",
            &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    let current = store.current_branch();

    let mut rows = String::new();
    if branches.is_empty() {
        rows.push_str(r#"<tr><td colspan="3" class="empty">no branches yet — try `lex publish &lt;file.lex&gt;`</td></tr>"#);
    } else {
        for name in &branches {
            let head = store.get_branch(name).ok().flatten()
                .and_then(|b| b.head_op)
                .unwrap_or_else(|| "—".into());
            let predicate = store.get_branch(name).ok().flatten()
                .and_then(|b| b.predicate)
                .map(|_| r#"<span class="tag">predicate</span>"#)
                .unwrap_or_default();
            let marker = if *name == current {
                r#"<span class="tag ok">current</span>"#
            } else { "" };
            rows.push_str(&format!(
                r#"<tr><td><a href="/web/branch/{n}">{n}</a> {marker}{predicate}</td><td class="mono">{h:.16}…</td></tr>"#,
                n = esc(name), marker = marker, predicate = predicate, h = esc(&head),
            ));
        }
    }
    let body = format!(
        r#"<h1>branches</h1>
<table><thead><tr><th>name</th><th>head_op</th></tr></thead><tbody>{rows}</tbody></table>"#,
    );
    html_response(200, page("branches", "/web/branches", &body))
}

// ---- Branch detail (unchanged shape) -----------------------------

pub(crate) fn branch_handler(state: &State, name: &str) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let head_map = match store.branch_head(name) {
        Ok(m) => m,
        Err(e) => return html_response(404, page("error", "",
            &format!(r#"<nav class="crumb"><a href="/web/branches">branches</a> / {}</nav><pre>{}</pre>"#,
                esc(name), esc(&e.to_string())))),
    };
    let mut rows = String::new();
    if head_map.is_empty() {
        rows.push_str(r#"<tr><td colspan="2" class="empty">no fns on this branch</td></tr>"#);
    } else {
        for (sig, stage_id) in &head_map {
            let fn_name = lookup_name_for_stage(&store, stage_id).unwrap_or_else(|| "—".into());
            rows.push_str(&format!(
                r#"<tr><td>{name}</td><td class="mono"><a href="/web/stage/{sid}" title="sig {sig}">{sid:.16}…</a></td></tr>"#,
                name = esc(&fn_name), sid = esc(stage_id), sig = esc(sig),
            ));
        }
    }
    let body = format!(
        r#"<nav class="crumb"><a href="/web/branches">branches</a> / <strong>{n}</strong></nav>
<h1>{n}</h1>
<table><thead><tr><th>fn</th><th>stage</th></tr></thead><tbody>{rows}</tbody></table>"#,
        n = esc(name),
    );
    html_response(200, page(name, "", &body))
}

fn lookup_name_for_stage(store: &Store, stage_id: &str) -> Option<String> {
    store.get_metadata(stage_id).ok().map(|m| m.name)
}

// ---- Stage detail (unchanged shape) -------------------------------

pub(crate) fn stage_html_handler(state: &State, id: &str) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let meta = match store.get_metadata(id) {
        Ok(m) => m,
        Err(e) => return html_response(404, page("not found", "",
            &format!(r#"<nav class="crumb"><a href="/">activity</a></nav><pre>{}</pre>"#, esc(&e.to_string())))),
    };
    let status = store.get_status(id).map(|s| format!("{s:?}")).unwrap_or_else(|_| "?".into());
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return html_response(500, page("error", "",
            &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    let mut atts = log.list_for_stage(&id.to_string()).unwrap_or_default();
    atts.sort_by_key(|a| std::cmp::Reverse(a.timestamp));

    let mut att_rows = String::new();
    if atts.is_empty() {
        att_rows.push_str(r#"<tr><td colspan="4" class="empty">no attestations yet</td></tr>"#);
    } else {
        for a in &atts {
            let kind = kind_short(&a.kind);
            let (lbl, css) = result_short(&a.result);
            let detail = match &a.result {
                AttestationResult::Failed { detail }
                | AttestationResult::Inconclusive { detail } => esc(detail),
                AttestationResult::Passed => String::new(),
            };
            att_rows.push_str(&format!(
                r#"<tr>
                  <td><span class="tag kind">{kind}</span></td>
                  <td><span class="tag {css}">{lbl}</span> {detail}</td>
                  <td class="mono">{tool}@{ver}</td>
                  <td class="muted mono">{ts}</td>
                </tr>"#,
                kind = esc(&kind), css = css, lbl = lbl, detail = detail,
                tool = esc(&a.produced_by.tool),
                ver = esc(&a.produced_by.version),
                ts = a.timestamp,
            ));
        }
    }

    // Triage forms (lex-tea v3a/v3b/v3c, #172). Render only when
    // a user identifier is set via `LEX_TEA_USER`; without one the
    // handlers refuse the action anyway, so showing buttons would
    // be misleading. Single-user-mode for now; users.json-backed
    // auth is a follow-up.
    let actor = std::env::var("LEX_TEA_USER").ok();
    let triage_form = match &actor {
        Some(name) => format!(
            r#"<h2>triage</h2>
<p class="muted">Record a human decision against this stage.
All four actions write an auditable attestation under your name
queryable via <code>lex attest filter --kind &lt;kind&gt;</code>.
<strong>pin</strong> additionally activates the stage; the others
just record the decision. <strong>block</strong> prevents future
pins until <strong>unblock</strong> is recorded.</p>
<p class="muted">actor: <span class="mono">{actor}</span></p>
<form method="post" action="/web/stage/{id}/pin">
  <p><label for="pin-reason">pin reason:</label><br>
     <input type="text" id="pin-reason" name="reason" required
            placeholder="e.g. spec checker is wrong here, will revisit"
            style="width: 100%; padding: .4rem; font-family: inherit;"></p>
  <p><button type="submit"
       style="padding: .4rem 1rem; background: #b00; color: #fff; border: 0;
              border-radius: 3px; cursor: pointer;">pin to Active</button></p>
</form>
<form method="post" action="/web/stage/{id}/defer">
  <p><label for="defer-reason">defer reason:</label><br>
     <input type="text" id="defer-reason" name="reason" required
            placeholder="e.g. low priority, revisit next sprint"
            style="width: 100%; padding: .4rem; font-family: inherit;"></p>
  <p><button type="submit"
       style="padding: .4rem 1rem; background: #555; color: #fff; border: 0;
              border-radius: 3px; cursor: pointer;">defer</button></p>
</form>
<form method="post" action="/web/stage/{id}/block">
  <p><label for="block-reason">block reason:</label><br>
     <input type="text" id="block-reason" name="reason" required
            placeholder="e.g. blocks until external review lands"
            style="width: 100%; padding: .4rem; font-family: inherit;"></p>
  <p><button type="submit"
       style="padding: .4rem 1rem; background: #800; color: #fff; border: 0;
              border-radius: 3px; cursor: pointer;">block</button></p>
</form>
<form method="post" action="/web/stage/{id}/unblock">
  <p><label for="unblock-reason">unblock reason:</label><br>
     <input type="text" id="unblock-reason" name="reason" required
            placeholder="e.g. external review landed"
            style="width: 100%; padding: .4rem; font-family: inherit;"></p>
  <p><button type="submit"
       style="padding: .4rem 1rem; background: #060; color: #fff; border: 0;
              border-radius: 3px; cursor: pointer;">unblock</button></p>
</form>"#,
            id = esc(id),
            actor = esc(name),
        ),
        None => r#"<h2>triage</h2>
<p class="muted">Set <code>LEX_TEA_USER=&lt;name&gt;</code> in the
server's environment to enable human triage actions (pin / defer /
block / unblock). The attestation log records every decision under
that name.</p>"#.into(),
    };

    let body = format!(
        r#"<nav class="crumb"><a href="/">activity</a> / <strong class="mono">{id_short}…</strong></nav>
<h1>{name}</h1>
<table>
  <tr><th>name</th><td>{name}</td></tr>
  <tr><th>sig_id</th><td class="mono">{sig:.16}…</td></tr>
  <tr><th>stage_id</th><td class="mono">{stage:.16}…</td></tr>
  <tr><th>status</th><td>{status}</td></tr>
  <tr><th>published</th><td class="muted">{ts}</td></tr>
</table>
<h2>attestations ({n})</h2>
<table>
  <thead><tr><th>kind</th><th>result</th><th>by</th><th>ts</th></tr></thead>
  <tbody>{att_rows}</tbody>
</table>
{triage_form}"#,
        id_short = esc(&format!("{id:.16}")),
        name = esc(&meta.name),
        sig = esc(&meta.sig_id),
        stage = esc(&meta.stage_id),
        status = esc(&status),
        ts = meta.published_at,
        n = atts.len(),
    );
    html_response(200, page(&meta.name, "", &body))
}

/// Which human-triage action a `POST /web/stage/<id>/<verb>` is
/// requesting. Drives both the recorded `AttestationKind` and
/// whether the stage gets activated.
#[derive(Clone, Copy)]
pub(crate) enum WebStageDecision {
    Pin,
    Defer,
    Block,
    Unblock,
}

impl WebStageDecision {
    fn verb(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Defer => "defer",
            Self::Block => "block",
            Self::Unblock => "unblock",
        }
    }
    fn tool(self) -> &'static str {
        match self {
            Self::Pin => "lex-tea pin",
            Self::Defer => "lex-tea defer",
            Self::Block => "lex-tea block",
            Self::Unblock => "lex-tea unblock",
        }
    }
    fn kind(self, actor: String, reason: String) -> AttestationKind {
        match self {
            Self::Pin => AttestationKind::Override {
                actor, reason, target_attestation_id: None,
            },
            Self::Defer => AttestationKind::Defer { actor, reason },
            Self::Block => AttestationKind::Block { actor, reason },
            Self::Unblock => AttestationKind::Unblock { actor, reason },
        }
    }
}

/// `POST /web/stage/<id>/{pin,defer,block,unblock}` — handles
/// the human-triage form submissions. Picks the actor from the
/// `X-Lex-User` request header (preferred, set by the proxy or
/// programmatic client) or falls back to the `LEX_TEA_USER`
/// server env. If `<store>/users.json` exists the resolved name
/// must be in it. Records the appropriate attestation, and (for
/// `pin` only) activates the stage. Redirects back to the stage
/// page so a refresh doesn't repost the action.
pub(crate) fn stage_decision_handler(
    state: &State,
    id: &str,
    body: &str,
    decision: WebStageDecision,
    x_lex_user: Option<&str>,
) -> Response<Cursor<Vec<u8>>> {
    let actor = x_lex_user
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LEX_TEA_USER").ok())
        .filter(|s| !s.is_empty());
    let Some(actor) = actor else {
        return html_response(403, page("forbidden", "",
            r#"<nav class="crumb"><a href="/">activity</a></nav>
<h1>forbidden</h1>
<p>No actor identified. Send <code>X-Lex-User</code> on the request, or set <code>LEX_TEA_USER</code> on the server.</p>"#));
    };
    if let Ok(Some(users)) = lex_store::users::load(&state.root) {
        if !users.knows(&actor) {
            return html_response(403, page("forbidden", "",
                &format!(r#"<nav class="crumb"><a href="/">activity</a></nav>
<h1>forbidden</h1>
<p>Actor <code>{}</code> is not listed in <code>users.json</code>.</p>"#,
                    esc(&actor))));
        }
    }
    let reason = body.split('&')
        .find_map(|pair| pair.strip_prefix("reason="))
        .map(percent_decode)
        .filter(|s| !s.trim().is_empty());
    let Some(reason) = reason else {
        return html_response(400, page("bad request", "",
            r#"<nav class="crumb"><a href="/">activity</a></nav>
<h1>bad request</h1>
<p>This form requires a <code>reason</code>.</p>"#));
    };

    let store = state.store.lock().unwrap();
    if store.get_metadata(id).is_err() {
        return html_response(404, page("not found", "",
            &format!(r#"<nav class="crumb"><a href="/">activity</a></nav><pre>unknown stage `{}`</pre>"#,
                esc(id))));
    }
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return html_response(500, page("error", "",
            &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    if matches!(decision, WebStageDecision::Pin) {
        // Refuse to pin a blocked stage — same gate as the CLI.
        // Web users have to record an Unblock first.
        let existing = log.list_for_stage(&id.to_string()).unwrap_or_default();
        if lex_vcs::is_stage_blocked(&existing) {
            return html_response(409, page("blocked", "",
                &format!(r#"<nav class="crumb"><a href="/">activity</a> /
<a href="/web/stage/{id}">{id_short}…</a></nav>
<h1>stage is blocked</h1>
<p>Record an <strong>unblock</strong> first, then re-try the pin.</p>"#,
                    id = esc(id),
                    id_short = esc(&format!("{id:.16}")))));
        }
        if let Err(e) = store.activate(id) {
            return html_response(500, page("error", "",
                &format!("<pre>activate: {}</pre>", esc(&e.to_string()))));
        }
    }
    let attestation = lex_vcs::Attestation::new(
        id.to_string(), None, None,
        decision.kind(actor, reason),
        AttestationResult::Passed,
        lex_vcs::ProducerDescriptor {
            tool: decision.tool().into(),
            version: env!("CARGO_PKG_VERSION").into(),
            model: None,
        },
        None,
    );
    if let Err(e) = log.put(&attestation) {
        return html_response(500, page("error", "",
            &format!("<pre>persist {}: {}</pre>",
                decision.verb(), esc(&e.to_string()))));
    }
    Response::from_data(Vec::new())
        .with_status_code(303)
        .with_header(
            tiny_http::Header::from_bytes(&b"Location"[..],
                format!("/web/stage/{}", id).as_bytes()).unwrap(),
        )
}

/// Inline percent-decoder for x-www-form-urlencoded values. Just
/// enough to handle `+` → space and `%XX` → byte. Doesn't support
/// nested encoding or Unicode normalization; the override form
/// has one field of free text and that's all this needs.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

// ---- shared helpers -----------------------------------------------

/// Compact label for an `AttestationKind`. Used by every renderer
/// so the activity feed and the stage page show the same string.
fn kind_short(k: &AttestationKind) -> String {
    match k {
        AttestationKind::TypeCheck => "TypeCheck".into(),
        AttestationKind::EffectAudit => "EffectAudit".into(),
        AttestationKind::Spec { spec_id, .. } => format!("Spec({spec_id:.12}…)"),
        AttestationKind::Examples { count, .. } => format!("Examples({count})"),
        AttestationKind::DiffBody { input_count, .. } => format!("DiffBody({input_count})"),
        AttestationKind::SandboxRun { effects } => {
            let names: Vec<&str> = effects.iter().map(String::as_str).collect();
            format!("SandboxRun([{}])", names.join(","))
        }
        AttestationKind::Override { actor, .. } => format!("Override({actor})"),
        AttestationKind::Defer { actor, .. } => format!("Defer({actor})"),
        AttestationKind::Block { actor, .. } => format!("Block({actor})"),
        AttestationKind::Unblock { actor, .. } => format!("Unblock({actor})"),
        AttestationKind::Trace { run_id, root_target } => {
            format!("Trace({root_target}@{run_id:.12}…)")
        }
    }
}

/// `(label, css class)` pair for the result tag.
fn result_short(r: &AttestationResult) -> (&'static str, &'static str) {
    match r {
        AttestationResult::Passed => ("passed", "ok"),
        AttestationResult::Failed { .. } => ("failed", "fail"),
        AttestationResult::Inconclusive { .. } => ("inconclusive", "inc"),
    }
}
