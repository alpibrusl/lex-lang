//! `lex-tea` v1 — a minimal HTML browser over the existing
//! lex-vcs JSON endpoints. Three read-only pages:
//!
//! * `/`                    — list of branches
//! * `/web/branch/<name>`   — list of fns on a branch with stage_id links
//! * `/web/stage/<id>`      — stage metadata + attestation trail
//!
//! Wired into the same `tiny_http` server as the JSON API
//! (no extra binary, no new port). The point is to expose
//! the JSON API's structure to humans without a SPA build
//! pipeline. CSS is one short embedded blob; no JS.

use lex_store::Store;
use std::io::Cursor;
use tiny_http::{Header, Response};

use crate::handlers::State;

/// Embedded so a `cargo build` produces a fully-self-contained
/// binary. ~1KB; not worth a separate file dance.
const STYLE: &str = r#"
* { box-sizing: border-box; }
body { font: 14px/1.5 -apple-system, system-ui, sans-serif;
       max-width: 880px; margin: 2rem auto; padding: 0 1rem; color: #222; }
h1 { font-weight: 600; margin: 0 0 1.5rem; }
h2 { font-weight: 500; margin: 1.5rem 0 .5rem; color: #444; }
a { color: #0a5; text-decoration: none; }
a:hover { text-decoration: underline; }
nav { font-size: 13px; color: #888; margin-bottom: 1rem; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { text-align: left; padding: .35rem .6rem; border-bottom: 1px solid #eee; }
th { color: #666; font-weight: 500; }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
.muted { color: #999; }
.tag { display: inline-block; padding: 1px 6px; font-size: 11px; border-radius: 3px;
       background: #eef; color: #335; margin-right: 4px; }
.tag.ok { background: #dfd; color: #060; }
.tag.fail { background: #fdd; color: #800; }
.tag.inc { background: #ffd; color: #850; }
pre { background: #f6f6f6; padding: .8rem; overflow-x: auto; font-size: 12px; border-radius: 3px; }
.empty { color: #aaa; font-style: italic; }
"#;

fn html_response(status: u16, body: String) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.into_bytes())
        .with_status_code(status)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
        )
}

fn page(title: &str, breadcrumb: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title} — lex-tea</title>
<style>{STYLE}</style>
</head>
<body>
<nav>{breadcrumb}</nav>
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

/// `GET /` — branch list.
pub(crate) fn index_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let branches = match store.list_branches() {
        Ok(b) => b,
        Err(e) => return html_response(500, page("error", "/", &format!("<pre>{}</pre>", esc(&e.to_string())))),
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
            } else {
                ""
            };
            rows.push_str(&format!(
                r#"<tr><td><a href="/web/branch/{n}">{n}</a> {marker}{predicate}</td><td class="mono">{h:.16}…</td></tr>"#,
                n = esc(name),
                marker = marker,
                predicate = predicate,
                h = esc(&head),
            ));
        }
    }
    let body = format!(
        r#"<h1>lex-tea</h1>
<p class="muted">Read-only browser over <code>lex-vcs</code>. JSON API at <code>/v1/*</code>.</p>
<h2>branches</h2>
<table><thead><tr><th>name</th><th>head_op</th></tr></thead><tbody>{rows}</tbody></table>"#,
    );
    html_response(200, page("branches", "/", &body))
}

/// `GET /web/branch/<name>` — fns on a branch.
pub(crate) fn branch_handler(state: &State, name: &str) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let head_map = match store.branch_head(name) {
        Ok(m) => m,
        Err(e) => return html_response(404, page("error", &nav_for_branch(name), &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };

    let mut rows = String::new();
    if head_map.is_empty() {
        rows.push_str(r#"<tr><td colspan="2" class="empty">no fns on this branch</td></tr>"#);
    } else {
        // `head_map` is SigId → StageId. Render the stage_id as the
        // primary link; sig_id is an internal identifier we don't
        // surface yet beyond a hover.
        for (sig, stage_id) in &head_map {
            let fn_name = lookup_name_for_stage(&store, stage_id).unwrap_or_else(|| "—".into());
            rows.push_str(&format!(
                r#"<tr><td>{name}</td><td class="mono"><a href="/web/stage/{sid}" title="sig {sig}">{sid:.16}…</a></td></tr>"#,
                name = esc(&fn_name),
                sid = esc(stage_id),
                sig = esc(sig),
            ));
        }
    }
    let body = format!(
        r#"<h1>{n}</h1>
<table><thead><tr><th>fn</th><th>stage</th></tr></thead><tbody>{rows}</tbody></table>"#,
        n = esc(name),
    );
    html_response(200, page(name, &nav_for_branch(name), &body))
}

fn lookup_name_for_stage(store: &Store, stage_id: &str) -> Option<String> {
    store.get_metadata(stage_id).ok().map(|m| m.name)
}

fn nav_for_branch(name: &str) -> String {
    format!(r#"<a href="/">branches</a> / <strong>{}</strong>"#, esc(name))
}

fn nav_for_stage(stage_id: &str) -> String {
    format!(
        r#"<a href="/">branches</a> / <strong class="mono">{}</strong>"#,
        esc(&format!("{stage_id:.16}…")),
    )
}

/// `GET /web/stage/<id>` — stage metadata + attestation trail.
pub(crate) fn stage_html_handler(state: &State, id: &str) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let meta = match store.get_metadata(id) {
        Ok(m) => m,
        Err(e) => return html_response(404, page("not found", &nav_for_stage(id), &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    let status = store.get_status(id).map(|s| format!("{s:?}")).unwrap_or_else(|_| "?".into());
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return html_response(500, page("error", &nav_for_stage(id), &format!("<pre>{}</pre>", esc(&e.to_string())))),
    };
    let mut atts = log.list_for_stage(&id.to_string()).unwrap_or_default();
    atts.sort_by_key(|a| std::cmp::Reverse(a.timestamp));

    let mut att_rows = String::new();
    if atts.is_empty() {
        att_rows.push_str(r#"<tr><td colspan="4" class="empty">no attestations yet</td></tr>"#);
    } else {
        for a in &atts {
            let kind = match &a.kind {
                lex_vcs::AttestationKind::TypeCheck => "TypeCheck".to_string(),
                lex_vcs::AttestationKind::EffectAudit => "EffectAudit".to_string(),
                lex_vcs::AttestationKind::Spec { spec_id, .. } => format!("Spec({spec_id:.12}…)"),
                lex_vcs::AttestationKind::Examples { count, .. } => format!("Examples({count})"),
                lex_vcs::AttestationKind::DiffBody { input_count, .. } => format!("DiffBody({input_count})"),
                lex_vcs::AttestationKind::SandboxRun { effects } => {
                    let names: Vec<&str> = effects.iter().map(|s| s.as_str()).collect();
                    format!("SandboxRun([{}])", names.join(","))
                }
            };
            let (result, css) = match &a.result {
                lex_vcs::AttestationResult::Passed => ("passed".to_string(), "ok"),
                lex_vcs::AttestationResult::Failed { detail } => (format!("failed: {detail}"), "fail"),
                lex_vcs::AttestationResult::Inconclusive { detail } => {
                    (format!("inconclusive: {detail}"), "inc")
                }
            };
            att_rows.push_str(&format!(
                r#"<tr><td>{kind}</td><td><span class="tag {css}">{result}</span></td><td class="mono">{tool}@{ver}</td><td class="muted mono">{ts}</td></tr>"#,
                kind = esc(&kind),
                css = css,
                result = esc(&result),
                tool = esc(&a.produced_by.tool),
                ver = esc(&a.produced_by.version),
                ts = a.timestamp,
            ));
        }
    }

    let body = format!(
        r#"<h1>{name}</h1>
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
</table>"#,
        name = esc(&meta.name),
        sig = esc(&meta.sig_id),
        stage = esc(&meta.stage_id),
        status = esc(&status),
        ts = meta.published_at,
        n = atts.len(),
    );
    html_response(200, page(&meta.name, &nav_for_stage(id), &body))
}
