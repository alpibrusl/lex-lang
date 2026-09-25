//! git plumbing for the importer: objects only, never a checkout.

use anyhow::{anyhow, bail, Context, Result};
use lex_vcs::Person;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// A `git` invocation pinned to `dir`, with the caller's repo-selecting
/// environment scrubbed (so a stray `GIT_DIR` cannot redirect the read) and
/// nothing ever prompting.
pub(super) fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    scrub(&mut c);
    c
}

/// The environment every importer `git` runs under.
pub(super) fn scrub(c: &mut Command) {
    for v in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        c.env_remove(v);
    }
    c.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C").env("GIT_OPTIONAL_LOCKS", "0");
}

pub(super) fn git_out(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git_cmd(dir)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}` (is git installed?)", args.join(" ")))?;
    if !out.status.success() {
        bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

pub(super) fn git_line(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_out(dir, args)?).trim().to_string())
}

/// The exit status of a `git` invocation (for yes/no plumbing such as
/// `merge-base --is-ancestor`), `Err` only when git could not run at all.
pub(super) fn git_ok(dir: &Path, args: &[&str]) -> Result<bool> {
    let st = git_cmd(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("running `git {}` (is git installed?)", args.join(" ")))?;
    Ok(st.success())
}

/// One long-lived `git cat-file --batch`: blob bytes straight from the object
/// database, with no smudge/eol/attributes processing.
pub(super) struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// How many blob bodies were read: the perf-shape metric (`stats.blob_reads`).
    pub reads: u64,
}

impl CatFile {
    pub(super) fn spawn(dir: &Path) -> Result<CatFile> {
        let mut child = git_cmd(dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning `git cat-file --batch`")?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(CatFile { child, stdin, stdout, reads: 0 })
    }

    pub(super) fn read(&mut self, oid: &str) -> Result<Vec<u8>> {
        self.stdin
            .write_all(format!("{oid}\n").as_bytes())
            .and_then(|_| self.stdin.flush())
            .context("writing to `git cat-file --batch`")?;
        let mut header = String::new();
        self.stdout.read_line(&mut header).context("reading `git cat-file --batch`")?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        let size: usize = match parts.as_slice() {
            [_, "blob", size] => size.parse().map_err(|_| anyhow!("bad cat-file header `{}`", header.trim()))?,
            [_, "missing"] => bail!("git object {oid} is missing from the repository"),
            _ => bail!("unexpected cat-file header `{}` for {oid}", header.trim()),
        };
        let mut buf = vec![0u8; size];
        self.stdout.read_exact(&mut buf).with_context(|| format!("reading git object {oid}"))?;
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl).context("reading cat-file terminator")?;
        self.reads += 1;
        Ok(buf)
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One long-lived `git cat-file --batch-check`: an object's size without its
/// body, so the per-file limit is enforced before any byte is read.
pub(super) struct ObjInfo {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pub queries: u64,
}

impl ObjInfo {
    pub(super) fn spawn(dir: &Path) -> Result<ObjInfo> {
        let mut child = git_cmd(dir)
            .args(["cat-file", "--batch-check"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning `git cat-file --batch-check`")?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(ObjInfo { child, stdin, stdout, queries: 0 })
    }

    pub(super) fn size(&mut self, oid: &str) -> Result<u64> {
        self.stdin
            .write_all(format!("{oid}\n").as_bytes())
            .and_then(|_| self.stdin.flush())
            .context("writing to `git cat-file --batch-check`")?;
        let mut line = String::new();
        self.stdout.read_line(&mut line).context("reading `git cat-file --batch-check`")?;
        self.queries += 1;
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            [_, "blob", size] => size.parse().map_err(|_| anyhow!("bad cat-file size `{}`", line.trim())),
            [_, "missing"] => bail!("git object {oid} is missing from the repository"),
            _ => bail!("unexpected cat-file --batch-check line `{}` for {oid}", line.trim()),
        }
    }
}

impl Drop for ObjInfo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One changed (or listed) tree entry. `mode == 0` means the path was deleted.
#[derive(Debug, Clone)]
pub(super) struct RawChange {
    /// Raw path bytes (git paths are bytes; non-UTF-8 ones are refused).
    pub path: Vec<u8>,
    pub mode: u32,
    pub oid: String,
}

/// Every entry of `sha`'s tree (`git ls-tree -r`), as additions: the start of
/// a snapshot (a `--head-only` import, an incremental run's watermark).
pub(super) fn ls_tree_changes(dir: &Path, sha: &str) -> Result<Vec<RawChange>> {
    let raw = git_out(dir, &["ls-tree", "-r", "-z", "--full-tree", sha])?;
    let mut out = Vec::new();
    for rec in raw.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let tab = rec.iter().position(|&b| b == b'\t').ok_or_else(|| anyhow!("malformed ls-tree record"))?;
        let meta = String::from_utf8_lossy(&rec[..tab]).to_string();
        let f: Vec<&str> = meta.split_whitespace().collect();
        let [mode, _kind, oid] = f.as_slice() else {
            bail!("malformed ls-tree record `{meta}`");
        };
        out.push(RawChange {
            path: rec[tab + 1..].to_vec(),
            mode: u32::from_str_radix(mode, 8).map_err(|_| anyhow!("bad mode `{mode}`"))?,
            oid: oid.to_string(),
        });
    }
    Ok(out)
}

/// What changed between two commits' trees: `git diff-tree -r -z --raw
/// --no-renames`. A merge commit is diffed against the parent we name (the
/// first-parent line), which is what folds the side branch into one commit.
pub(super) fn diff_tree_changes(dir: &Path, old: &str, new: &str) -> Result<Vec<RawChange>> {
    let raw = git_out(
        dir,
        &["diff-tree", "-r", "-z", "--raw", "--no-renames", "--no-abbrev", "--root", old, new],
    )?;
    let mut out = Vec::new();
    let mut toks = raw.split(|&b| b == 0).filter(|t| !t.is_empty());
    while let Some(head) = toks.next() {
        // `:<oldmode> <newmode> <oldoid> <newoid> <status>` then the path.
        let head = String::from_utf8_lossy(head);
        let Some(h) = head.strip_prefix(':') else {
            bail!("malformed diff-tree record `{head}`");
        };
        let f: Vec<&str> = h.split_whitespace().collect();
        let [_om, nm, _oo, no, _st] = f.as_slice() else {
            bail!("malformed diff-tree record `{head}`");
        };
        let path = toks.next().ok_or_else(|| anyhow!("diff-tree record without a path"))?.to_vec();
        out.push(RawChange {
            path,
            mode: u32::from_str_radix(nm, 8).map_err(|_| anyhow!("bad mode `{nm}`"))?,
            oid: no.to_string(),
        });
    }
    Ok(out)
}

/// `git rev-list --first-parent --reverse <tip>`: the first-parent line of
/// history, oldest first.
pub(super) fn first_parent_chain(dir: &Path, tip: &str) -> Result<Vec<String>> {
    Ok(String::from_utf8_lossy(&git_out(dir, &["rev-list", "--first-parent", "--reverse", tip])?)
        .lines()
        .map(str::to_string)
        .collect())
}

/// A parsed git commit object.
#[derive(Debug, Clone)]
pub(super) struct CommitMeta {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: Person,
    pub committer: Person,
    /// The message verbatim (lossy UTF-8, so the decode is deterministic).
    pub message: String,
}

/// Parse `git cat-file commit <sha>` directly — never localized `git log`
/// output. Headers end at the first blank line; a continuation line (a
/// `gpgsig` / `mergetag` body) starts with a space and is skipped.
pub(super) fn read_commit(dir: &Path, sha: &str) -> Result<CommitMeta> {
    let raw = git_out(dir, &["cat-file", "commit", sha])?;
    let split = raw.windows(2).position(|w| w == b"\n\n");
    let (head, body) = match split {
        Some(i) => (&raw[..i], &raw[i + 2..]),
        None => (&raw[..], &raw[raw.len()..]),
    };
    let head = String::from_utf8_lossy(head);
    let (mut parents, mut author, mut committer) = (Vec::new(), None, None);
    for line in head.split('\n') {
        if line.starts_with(' ') {
            continue;
        }
        let (key, val) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "parent" => parents.push(val.to_string()),
            "author" if author.is_none() => author = Some(parse_person(val)?),
            "committer" if committer.is_none() => committer = Some(parse_person(val)?),
            _ => {}
        }
    }
    Ok(CommitMeta {
        sha: sha.to_string(),
        parents,
        author: author.ok_or_else(|| anyhow!("commit {sha} has no author"))?,
        committer: committer.ok_or_else(|| anyhow!("commit {sha} has no committer"))?,
        message: String::from_utf8_lossy(body).into_owned(),
    })
}

/// `Name <email> 1700000000 +0200` → [`Person`]. The zone is kept verbatim.
pub(super) fn parse_person(s: &str) -> Result<Person> {
    let gt = s.rfind('>').ok_or_else(|| anyhow!("malformed identity `{s}`"))?;
    let lt = s[..gt].rfind('<').ok_or_else(|| anyhow!("malformed identity `{s}`"))?;
    let name = s[..lt].trim_end().to_string();
    let email = s[lt + 1..gt].to_string();
    let mut rest = s[gt + 1..].split_whitespace();
    let when: i64 = rest
        .next()
        .and_then(|w| w.parse().ok())
        .ok_or_else(|| anyhow!("malformed identity date in `{s}`"))?;
    let tz = rest.next().unwrap_or("+0000").to_string();
    Ok(Person { name, email, when, tz })
}

/// The first line of a commit's message (empty if the commit is unreadable —
/// e.g. a parent behind a shallow boundary).
pub(super) fn subject_of(dir: &Path, sha: &str) -> String {
    git_line(dir, &["log", "-1", "--format=%s", sha]).unwrap_or_default()
}
