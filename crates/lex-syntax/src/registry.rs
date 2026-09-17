//! Mapping a **tenant-qualified registry** string to the hub's public,
//! unauthenticated read URLs (#893, #917).
//!
//! A registry dependency names its source as `host/tenant[/store]`:
//!
//! ```toml
//! [dependencies]
//! lex-nt = { registry = "vcs.lexlang.org/lex-official/lex-nt", version = "^1.2" }
//! ```
//!
//! The public package surface the hub serves is
//! `GET https://<host>/v1/public/<tenant>/<name>/…` (a named store is
//! selected with `?store=<store>`), and it needs no credentials for a
//! **public** package. The resolver (`lex pkg lock`/`install`) reads that
//! surface, so it must turn the `registry` string into those URLs rather
//! than hitting the authenticated, tenant-from-key `/v1/pkg/<name>/…` routes
//! (which return `401` to an anonymous resolver).
//!
//! A bare-host registry (`"vcs.lexlang.org"`, no tenant) can't address the
//! public surface — there's no tenant to scope to — so [`public`] returns
//! `None` and the caller falls back to the legacy `{registry}/v1/pkg/…`
//! form (which only ever worked for a single-tenant host anyway).

/// A registry resolved to its public read surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicRegistry {
    /// `https://<host>/v1/public/<tenant>` — the base every public read
    /// hangs off.
    pub base: String,
    /// The named store under the tenant, if the registry named one. Passed
    /// as `?store=<store>`; `None` reads the tenant's default (flat) store.
    pub store: Option<String>,
}

impl PublicRegistry {
    /// `…/v1/public/<tenant>/<name>/versions[?store=…]` — the release list.
    pub fn versions_url(&self, name: &str) -> String {
        self.with_store(format!("{}/{}/versions", self.base, name))
    }

    /// `…/v1/public/<tenant>/<name>/<version>/archive[?store=…]` — the
    /// package archive for a resolved version.
    pub fn archive_url(&self, name: &str, version: &str) -> String {
        self.with_store(format!("{}/{}/{}/archive", self.base, name, version))
    }

    /// `…/v1/public/<tenant>/<name>/<version>/contract[?store=…]` — the
    /// signed capability contract, when the surface serves one. (The public
    /// surface may not; callers treat a `404` as "unsigned".)
    pub fn contract_url(&self, name: &str, version: &str) -> String {
        self.with_store(format!("{}/{}/{}/contract", self.base, name, version))
    }

    fn with_store(&self, url: String) -> String {
        match &self.store {
            Some(s) => format!("{url}?store={s}"),
            None => url,
        }
    }
}

/// Parse a `host/tenant[/store]` registry (scheme optional, trailing slash
/// tolerated) into its public read surface. Returns `None` for a bare-host
/// registry with no tenant segment — the caller then uses the legacy
/// `{registry}/v1/pkg/…` form.
pub fn public(registry: &str) -> Option<PublicRegistry> {
    let trimmed = registry.trim().trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("https", trimmed),
    };
    let mut segs = rest.split('/').filter(|s| !s.is_empty());
    let host = segs.next()?;
    let tenant = segs.next()?; // no tenant → not a public-addressable registry
    if host.is_empty() || tenant.is_empty() {
        return None;
    }
    let store = segs.next().map(str::to_string);
    Some(PublicRegistry {
        base: format!("{scheme}://{host}/v1/public/{tenant}"),
        store,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_store_registry() {
        let r = public("vcs.lexlang.org/alpibrusl").unwrap();
        assert_eq!(r.base, "https://vcs.lexlang.org/v1/public/alpibrusl");
        assert_eq!(r.store, None);
        assert_eq!(
            r.versions_url("lex-agent"),
            "https://vcs.lexlang.org/v1/public/alpibrusl/lex-agent/versions"
        );
        assert_eq!(
            r.archive_url("lex-agent", "0.1.0"),
            "https://vcs.lexlang.org/v1/public/alpibrusl/lex-agent/0.1.0/archive"
        );
    }

    #[test]
    fn named_store_registry_adds_store_query() {
        let r = public("vcs.lexlang.org/lex-official/lex-nt").unwrap();
        assert_eq!(r.base, "https://vcs.lexlang.org/v1/public/lex-official");
        assert_eq!(r.store.as_deref(), Some("lex-nt"));
        assert_eq!(
            r.versions_url("lex-nt"),
            "https://vcs.lexlang.org/v1/public/lex-official/lex-nt/versions?store=lex-nt"
        );
        assert_eq!(
            r.contract_url("lex-nt", "1.2.0"),
            "https://vcs.lexlang.org/v1/public/lex-official/lex-nt/1.2.0/contract?store=lex-nt"
        );
    }

    #[test]
    fn explicit_scheme_is_preserved() {
        let r = public("http://localhost:4040/acme").unwrap();
        assert_eq!(r.base, "http://localhost:4040/v1/public/acme");
    }

    #[test]
    fn bare_host_has_no_public_surface() {
        assert_eq!(public("vcs.lexlang.org"), None);
        assert_eq!(public("https://vcs.lexlang.org"), None);
        assert_eq!(public(""), None);
    }
}
