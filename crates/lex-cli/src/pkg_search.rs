//! `lex pkg search <query…>` — discover existing Lex packages before
//! hand-rolling one.
//!
//! Until this existed the only ways to learn that a router, a JWT library or
//! an ORM already ships as a package were to know the org, browse GitHub by
//! hand, or read the front-door site — none of which a coding agent in an
//! empty `lex init` project does. The command answers "what already exists
//! for X" from inside the toolchain, and prints the exact `lex.toml` line to
//! add.
//!
//! Backend: GitHub's repository search, scoped to the package org
//! (`alpibrusl` by default; `--org` for another). It is always current, needs
//! no index to keep fresh, and — unlike the hub's public listing — sees every
//! public package (the `lex-official` tenant lists nothing anonymously).
//! Search matches repo name, description and README, so a package with no
//! description (`lex-web` today) is still found by what it says it does.
//! Each hit's `lex.toml` is then fetched: it drops non-packages, supplies the
//! authoritative name/version/description and the toolchain floor.
//!
//! Environment:
//!   GITHUB_TOKEN            raises the search rate limit (10 → 30 req/min)
//!   LEX_PKG_SEARCH_API      API base   (default https://api.github.com)
//!   LEX_PKG_RAW_BASE        raw-file base (default https://raw.githubusercontent.com)
//!
//! The base overrides exist for tests and mirrors; the token is only ever
//! sent to the default GitHub hosts.

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const DEFAULT_API: &str = "https://api.github.com";
const DEFAULT_RAW: &str = "https://raw.githubusercontent.com";
const DEFAULT_ORG: &str = "alpibrusl";
const DEFAULT_LIMIT: usize = 8;
const BROWSE_URL: &str = "https://github.com/orgs/alpibrusl/repositories?q=lex-&type=public";

#[derive(Debug, PartialEq)]
struct Args {
    terms: Vec<String>,
    json: bool,
    limit: usize,
    org: String,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut words: Vec<&str> = Vec::new();
    let mut json = false;
    let mut limit = DEFAULT_LIMIT;
    let mut org = DEFAULT_ORG.to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // POSIX end-of-flags: everything after is query text, so a caller
            // (an agent tool) can pass a model-written query without it being
            // read as a flag.
            "--" => {
                words.extend(args[i + 1..].iter().map(|w| w.as_str()));
                break;
            }
            "--json" => json = true,
            "--limit" => {
                i += 1;
                limit = args
                    .get(i)
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| (1..=30).contains(n))
                    .ok_or_else(|| anyhow::anyhow!("--limit takes a number from 1 to 30"))?;
            }
            "--org" => {
                i += 1;
                org = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("--org requires a value"))?;
            }
            flag if flag.starts_with("--") => bail!("unknown flag `{flag}`"),
            word => words.push(word),
        }
        i += 1;
    }
    if org.is_empty() || !org.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("--org must be a GitHub organisation name");
    }
    let terms = query_terms(&words);
    if terms.is_empty() {
        bail!(
            "usage: lex pkg search <query…> [--json] [--limit N] [--org ORG]\n\
             e.g.   lex pkg search router      (or: http server, jwt, sql orm)"
        );
    }
    Ok(Args { terms, json, limit, org })
}

/// Lower-cased search terms. Anything outside `[a-z0-9._-]` is a separator, so
/// a query can never smuggle a GitHub qualifier (`org:`, `user:`, …) or a
/// boolean operator into the search string.
fn query_terms(words: &[&str]) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for w in words {
        for t in w
            .to_ascii_lowercase()
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'))
        {
            let t = t.trim_matches(|c| c == '.' || c == '-' || c == '_');
            if t.len() >= 2 && !STOPWORDS.contains(&t) && !terms.iter().any(|x| x == t) {
                terms.push(t.to_string());
            }
        }
    }
    // GitHub allows at most five boolean operators per query, and an "any of
    // these" search over N terms spends N-1 of them.
    terms.truncate(MAX_TERMS);
    terms
}

/// Words that match every README and would drown the real terms — and the
/// boolean keywords, which must never reach GitHub as operators.
const STOPWORDS: &[&str] = &[
    "or", "and", "not", "the", "with", "for", "of", "to", "in", "on", "an", "is", "it", "as", "by", "at", "from",
];
const MAX_TERMS: usize = 6;

fn search_query(terms: &[String], org: &str, joiner: &str) -> String {
    format!("{} org:{org} fork:false in:name,description,readme", terms.join(joiner))
}

// ── GitHub payloads ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    items: Vec<Repo>,
}

#[derive(Debug, Deserialize, Clone)]
struct Repo {
    name: String,
    full_name: String,
    html_url: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_branch")]
    default_branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

#[derive(Debug, PartialEq, Clone)]
struct Hit {
    name: String,
    description: String,
    git: String,
    version: String,
    lex_floor: String,
    /// Search rank as GitHub returned it (0 = best); tie-break for `score`.
    order: usize,
}

impl Hit {
    fn toml_line(&self) -> String {
        format!("{} = {{ git = \"{}\" }}", self.name, self.git)
    }

    fn add_command(&self) -> String {
        format!("lex pkg add {} --git {}", self.name, self.git)
    }
}

/// Name matches outrank description matches; both outrank a README-only hit
/// (which scores 0 and falls back to GitHub's own order).
fn score(h: &Hit, terms: &[String]) -> usize {
    let name = h.name.to_ascii_lowercase();
    let desc = h.description.to_ascii_lowercase();
    terms
        .iter()
        .map(|t| (if name.contains(t.as_str()) { 4 } else { 0 }) + (if desc.contains(t.as_str()) { 2 } else { 0 }))
        .sum()
}

fn rank(mut hits: Vec<Hit>, terms: &[String]) -> Vec<Hit> {
    hits.sort_by(|a, b| score(b, terms).cmp(&score(a, terms)).then(a.order.cmp(&b.order)));
    hits
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

struct Backend {
    api: String,
    raw: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl Backend {
    fn from_env() -> Self {
        let api = std::env::var("LEX_PKG_SEARCH_API").unwrap_or_else(|_| DEFAULT_API.into());
        let raw = std::env::var("LEX_PKG_RAW_BASE").unwrap_or_else(|_| DEFAULT_RAW.into());
        let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .build()
            .into();
        Backend { api: api.trim_end_matches('/').into(), raw: raw.trim_end_matches('/').into(), token, agent }
    }

    fn get(&self, url: &str, base: &str, default_base: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let mut req = self
            .agent
            .get(url)
            .header("User-Agent", concat!("lex-cli/", env!("CARGO_PKG_VERSION")))
            .header("Accept", "application/vnd.github+json");
        if let (Some(tok), true) = (&self.token, base == default_base) {
            req = req.header("Authorization", &format!("Bearer {tok}"));
        }
        req.call().map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn search(&self, q: &str, per_page: usize) -> Result<Vec<Repo>> {
        let url = format!(
            "{}/search/repositories?q={}&per_page={per_page}",
            self.api,
            urlencode(q)
        );
        let resp = self.get(&url, &self.api, DEFAULT_API)?;
        let status = resp.status().as_u16();
        match status {
            200 => {
                let body = resp.into_body().read_to_string().map_err(|e| anyhow::anyhow!("reading search response: {e}"))?;
                Ok(serde_json::from_str::<SearchResponse>(&body)
                    .map_err(|e| anyhow::anyhow!("unexpected search response: {e}"))?
                    .items)
            }
            403 | 429 => bail!(
                "GitHub search rate limit reached — retry in a minute, or set GITHUB_TOKEN to raise it"
            ),
            s => bail!("GitHub search returned HTTP {s}"),
        }
    }

    /// `Ok(None)` when the repo has no `lex.toml` (not a Lex package).
    fn manifest(&self, repo: &Repo) -> Result<Option<lex_syntax::Manifest>> {
        let url = format!("{}/{}/{}/lex.toml", self.raw, repo.full_name, repo.default_branch);
        let resp = self.get(&url, &self.raw, DEFAULT_RAW)?;
        match resp.status().as_u16() {
            200 => {
                let body = resp.into_body().read_to_string().map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(toml::from_str::<lex_syntax::Manifest>(&body).ok())
            }
            404 => Ok(None),
            s => bail!("HTTP {s}"),
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Repo → hit, via its `lex.toml`. Repos without one are dropped; a manifest
/// fetch that fails for another reason keeps the repo's own metadata rather
/// than hiding a real package behind a transient error.
fn resolve(backend: &Backend, repos: Vec<Repo>) -> Vec<Hit> {
    std::thread::scope(|s| {
        let handles: Vec<_> = repos
            .iter()
            .enumerate()
            .map(|(order, repo)| {
                s.spawn(move || {
                    let m = match backend.manifest(repo) {
                        Ok(Some(m)) => Some(m),
                        Ok(None) => return None,
                        Err(_) => None,
                    };
                    let pkg = m.as_ref().and_then(|m| m.package.as_ref());
                    Some(Hit {
                        name: pkg.map(|p| p.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| repo.name.clone()),
                        description: pkg
                            .and_then(|p| p.description.clone())
                            .filter(|d| !d.trim().is_empty())
                            .or_else(|| repo.description.clone())
                            .unwrap_or_default(),
                        git: repo.html_url.clone(),
                        version: pkg.map(|p| p.version.clone()).unwrap_or_default(),
                        lex_floor: pkg.and_then(|p| p.lex.clone()).unwrap_or_default(),
                        order,
                    })
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    })
}

// ── command ───────────────────────────────────────────────────────────────────

pub fn cmd_search(args: &[String]) -> Result<()> {
    let a = parse_args(args)?;
    let backend = Backend::from_env();
    // Fetch a wider pool than we print: non-packages are dropped and the
    // re-rank can promote a name match from further down.
    let pool = (a.limit * 3).min(30);

    let unavailable = |e: anyhow::Error| anyhow::anyhow!("package search unavailable: {e}\nbrowse instead: {BROWSE_URL}");

    let mut hits = resolve(&backend, backend.search(&search_query(&a.terms, &a.org, " "), pool).map_err(unavailable)?);
    let mut widened = false;
    if hits.len() < a.limit && a.terms.len() > 1 {
        // GitHub ANDs terms, and a multi-word query (an agent describing the
        // task) rarely has every word in one README — it either finds nothing
        // or a couple of accidental matches. When the strict search is thin,
        // add the "any of these" results; the re-rank below puts the ones
        // whose name/description match the most terms first.
        let known: Vec<String> = hits.iter().map(|h| h.git.clone()).collect();
        let more = backend.search(&search_query(&a.terms, &a.org, " OR "), pool).map_err(unavailable)?;
        let more: Vec<Repo> = more.into_iter().filter(|r| !known.contains(&r.html_url)).collect();
        let base = hits.len();
        for mut h in resolve(&backend, more) {
            h.order += base;
            hits.push(h);
            widened = true;
        }
    }
    let hits: Vec<Hit> = rank(hits, &a.terms).into_iter().take(a.limit).collect();

    if a.json {
        let results: Vec<_> = hits
            .iter()
            .map(|h| {
                json!({
                    "name": h.name, "description": h.description, "git": h.git,
                    "version": h.version, "lex": h.lex_floor,
                    "toml_line": h.toml_line(), "add_command": h.add_command(),
                })
            })
            .collect();
        let out = json!({ "query": a.terms.join(" "), "org": a.org, "matched_any_term": widened, "results": results });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print!("{}", render(&a, &hits, widened));
    }
    Ok(())
}

fn render(a: &Args, hits: &[Hit], widened: bool) -> String {
    let q = a.terms.join(" ");
    if hits.is_empty() {
        return format!(
            "no existing Lex package matches \"{q}\" in {} — nothing to reuse; try a broader word (e.g. \"http\" not \"http-router\"), or browse {BROWSE_URL}\n",
            a.org
        );
    }
    let mut out = format!(
        "{} package(s) for \"{q}\"{} — reuse before rebuilding:\n\n",
        hits.len(),
        if widened { " (matching any word)" } else { "" }
    );
    for h in hits {
        let ver = if h.version.is_empty() { String::new() } else { format!(" {}", h.version) };
        let desc = if h.description.is_empty() { "(no description)".to_string() } else { h.description.clone() };
        out.push_str(&format!("{}{ver} — {desc}\n", h.name));
        out.push_str(&format!("    {}\n", h.git));
        out.push_str(&format!("    add to lex.toml [dependencies]:  {}\n", h.toml_line()));
        out.push_str(&format!("    or:                              {}\n", h.add_command()));
        if !h.lex_floor.is_empty() {
            out.push_str(&format!("    needs lex >= {}\n", h.lex_floor));
        }
        out.push('\n');
    }
    out.push_str("then `lex pkg install`; read the package's README/src for its API.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn hit(name: &str, desc: &str, order: usize) -> Hit {
        Hit {
            name: name.into(),
            description: desc.into(),
            git: format!("https://github.com/o/{name}"),
            version: "1.0.0".into(),
            lex_floor: String::new(),
            order,
        }
    }

    #[test]
    fn terms_drop_qualifiers_operators_and_dupes() {
        assert_eq!(query_terms(&["REST", "API", "org:evil", "OR", "rest"]), s(&["rest", "api", "org", "evil"]));
        assert_eq!(query_terms(&["a", "x"]), Vec::<String>::new());
        assert_eq!(query_terms(&["http-router"]), s(&["http-router"]));
    }

    #[test]
    fn stopwords_and_term_cap() {
        assert_eq!(query_terms(&["a", "router", "with", "the", "jwt", "for"]), s(&["router", "jwt"]));
        let long: Vec<String> = (0..12).map(|i| format!("word{i}")).collect();
        let refs: Vec<&str> = long.iter().map(|w| w.as_str()).collect();
        assert_eq!(query_terms(&refs).len(), MAX_TERMS, "five OR operators is GitHub's ceiling");
    }

    #[test]
    fn query_cannot_change_the_org_scope() {
        let terms = query_terms(&["router", "org:someone-else"]);
        let q = search_query(&terms, "alpibrusl", " ");
        assert_eq!(q, "router org someone-else org:alpibrusl fork:false in:name,description,readme");
        assert_eq!(q.matches("org:").count(), 1, "only the scoping qualifier: {q}");
    }

    #[test]
    fn args_parse_flags_and_reject_junk() {
        let a = parse_args(&s(&["http", "server", "--json", "--limit", "3"])).unwrap();
        assert_eq!(a, Args { terms: s(&["http", "server"]), json: true, limit: 3, org: "alpibrusl".into() });
        assert!(parse_args(&s(&[])).is_err());
        assert!(parse_args(&s(&["x", "--limit", "0"])).is_err());
        assert!(parse_args(&s(&["router", "--org", "a b"])).is_err());
        assert!(parse_args(&s(&["router", "--bogus"])).is_err());
        // After `--` a flag-shaped word is just query text.
        let a = parse_args(&s(&["--json", "--", "--org", "jwt"])).unwrap();
        assert_eq!((a.json, a.org.as_str(), a.terms), (true, "alpibrusl", s(&["org", "jwt"])));
    }

    #[test]
    fn name_beats_description_beats_readme_only() {
        let terms = s(&["router"]);
        let ranked = rank(
            vec![hit("lex-ocpi", "OCPI adapter", 0), hit("lex-orm", "an ORM with a router", 1), hit("lex-router", "", 2)],
            &terms,
        );
        let names: Vec<_> = ranked.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["lex-router", "lex-orm", "lex-ocpi"]);
    }

    #[test]
    fn dependency_line_is_valid_lex_toml() {
        let h = hit("lex-web", "", 0);
        assert_eq!(h.toml_line(), r#"lex-web = { git = "https://github.com/o/lex-web" }"#);
        let m: lex_syntax::Manifest =
            toml::from_str(&format!("[package]\nname = \"x\"\n[dependencies]\n{}\n", h.toml_line())).unwrap();
        assert!(m.dependencies.contains_key("lex-web"));
    }
}
