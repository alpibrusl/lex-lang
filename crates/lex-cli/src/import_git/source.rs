//! Where the git repo comes from: a local path (read in place) or a URL
//! (cloned bare into a private scratch dir by `git` itself).
//!
//! Authentication is git's business, entirely: credential helpers, ssh-agent,
//! `GIT_ASKPASS` and `.netrc` all work because the clone IS `git clone`. The
//! importer never sees a token, never puts one in an argv it built, never
//! prompts (`GIT_TERMINAL_PROMPT=0`), and strips userinfo from every URL it
//! prints. The URL never reaches an intent or a hash: repo identity is the
//! root commit.

use super::git::{git_line, scrub};
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// A resolved source repository.
pub(super) struct Source {
    /// The repo to read (the bare clone for a URL).
    pub repo: PathBuf,
    /// Keeps the clone alive; removed on drop.
    _scratch: Option<TempDir>,
    /// What to echo in reports and errors: userinfo stripped.
    pub display: String,
    pub is_url: bool,
    pub shallow: bool,
}

/// Whether `s` names a remote (a URL or scp-style address) rather than a path.
pub(super) fn looks_like_url(s: &str) -> bool {
    if s.contains("://") {
        return true;
    }
    // scp-like `user@host:path` / `host:path` — but not an existing local path.
    !Path::new(s).exists() && s.contains(':') && !s.starts_with('/') && !s.starts_with('.')
}

/// Remove `userinfo@` from every `scheme://userinfo@host` in `s`, and the
/// `user@` of an scp-style address that is the whole string. Applied to the
/// source we echo and to git's own error text (which may quote the URL).
pub(super) fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // The authority runs to the first `/`, whitespace or quote.
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || c == '\'' || c == '"' || c == '?' || c == '#')
            .unwrap_or(tail.len());
        let (auth, after) = tail.split_at(end);
        match auth.rfind('@') {
            Some(at) => out.push_str(&auth[at + 1..]),
            None => out.push_str(auth),
        }
        rest = after;
    }
    out.push_str(rest);
    if !s.contains("://") && !s.contains(char::is_whitespace) {
        if let (Some(at), Some(colon)) = (out.find('@'), out.find(':')) {
            if at < colon {
                return out[at + 1..].to_string();
            }
        }
    }
    out
}

impl Source {
    /// Resolve `spec`. A URL is cloned bare (full history unless `depth`); a
    /// path must be the root of a non-shallow git repository.
    pub(super) fn open(spec: &str, depth: Option<u32>, branch: Option<&str>) -> Result<Source> {
        if looks_like_url(spec) {
            return Source::clone_url(spec, depth, branch);
        }
        if depth.is_some() {
            bail!(
                "--depth only applies to a URL source (a local repository is read in place, and a \
                 shallow local repository is refused); clone it yourself if you want a shallow import"
            );
        }
        let repo = PathBuf::from(spec);
        if !repo.is_dir() {
            bail!("{} is not a directory", repo.display());
        }
        git_line(&repo, &["rev-parse", "--git-dir"])
            .with_context(|| format!("{} is not a git repository", repo.display()))?;
        if !git_line(&repo, &["rev-parse", "--show-cdup"])?.is_empty() {
            bail!(
                "{} is inside a repository; pass the repository root (the package root is the repo root)",
                repo.display()
            );
        }
        if git_line(&repo, &["rev-parse", "--is-shallow-repository"])? == "true" {
            bail!(
                "{} is a shallow clone: the repo's identity is its root commit, which a shallow clone \
                 does not have, so the imported OpIds would not converge with other imports; \
                 `git fetch --unshallow` it (or import from its URL with --depth to accept that)",
                repo.display()
            );
        }
        Ok(Source { display: repo.display().to_string(), repo, _scratch: None, is_url: false, shallow: false })
    }

    fn clone_url(url: &str, depth: Option<u32>, branch: Option<&str>) -> Result<Source> {
        let display = redact(url);
        let scratch = tempfile::Builder::new()
            .prefix("lex-import-git-clone-")
            .tempdir()
            .context("creating scratch dir for the clone")?;
        let dest = scratch.path().join("repo.git");
        let mut cmd = Command::new("git");
        scrub(&mut cmd);
        // Only the transports a source can reasonably use; a caller who set
        // the variable themselves keeps their choice.
        if std::env::var_os("GIT_ALLOW_PROTOCOL").is_none() {
            cmd.env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh");
        }
        cmd.args(["clone", "--bare", "--quiet"]);
        if let Some(d) = depth {
            cmd.arg("--depth").arg(d.to_string()).arg("--single-branch");
            if let Some(b) = branch {
                cmd.arg("--branch").arg(b);
            }
        }
        // `--` so a hostile "URL" cannot smuggle a flag; the URL is an argv
        // element exactly as the user typed it (git reads credentials from
        // its own helpers, never from us).
        cmd.arg("--").arg(url).arg(&dest);
        let out = cmd.output().context("running `git clone` (is git installed?)")?;
        if !out.status.success() {
            bail!(
                "cloning {display} failed: {}",
                redact(String::from_utf8_lossy(&out.stderr).trim())
            );
        }
        let shallow = git_line(&dest, &["rev-parse", "--is-shallow-repository"])? == "true";
        if shallow && depth.is_none() {
            bail!(
                "{display} is itself a shallow repository: its root commit (the repo's identity) is \
                 not the real root, so imports would not converge; pass --depth N to accept that \
                 explicitly (the shallow boundary becomes the lineage root)"
            );
        }
        Ok(Source { repo: dest, _scratch: Some(scratch), display, is_url: true, shallow })
    }
}

/// `Err` message for a `--branch` the clone does not have.
pub(super) fn no_branch(branch: &str, src: &Source) -> anyhow::Error {
    anyhow!("no branch `{branch}` in {}", src.display)
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn userinfo_is_stripped_from_urls_and_error_text() {
        assert_eq!(redact("https://user:secret@host.example/x.git"), "https://host.example/x.git");
        assert_eq!(redact("https://tok@host/x"), "https://host/x");
        assert_eq!(redact("ssh://git@host:22/x"), "ssh://host:22/x");
        assert_eq!(redact("file:///tmp/x"), "file:///tmp/x");
        assert_eq!(
            redact("fatal: unable to access 'https://u:p@h/x/': Could not resolve host: h"),
            "fatal: unable to access 'https://h/x/': Could not resolve host: h"
        );
        assert_eq!(redact("git@github.com:a/b.git"), "github.com:a/b.git");
        assert_eq!(redact("a b@c"), "a b@c");
        // Two URLs in one string.
        assert_eq!(redact("https://a:b@x/ then https://c:d@y/"), "https://x/ then https://y/");
    }
}
