//! `fs_read` / `fs_walk` / `fs_write` effects: `fs.*` dispatch and the per-op path allow-list checks.

use super::*;

impl DefaultHandler {
    pub(super) fn dispatch_fs(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "exists" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                Ok(Value::Bool(std::path::Path::new(&path).exists()))
            }
            "is_file" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                Ok(Value::Bool(std::path::Path::new(&path).is_file()))
            }
            "is_dir" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                Ok(Value::Bool(std::path::Path::new(&path).is_dir()))
            }
            "stat" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::metadata(&path) {
                    Ok(md) => {
                        let mtime = md.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let mut rec = indexmap::IndexMap::new();
                        rec.insert("size".into(), Value::Int(md.len() as i64));
                        rec.insert("mtime".into(), Value::Int(mtime));
                        rec.insert("is_dir".into(), Value::Bool(md.is_dir()));
                        rec.insert("is_file".into(), Value::Bool(md.is_file()));
                        Ok(ok(Value::record_dynamic(rec)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("fs.stat `{path}`: {e}").into()))),
                }
            }
            "list_dir" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::read_dir(&path) {
                    Ok(rd) => {
                        let mut entries: Vec<Value> = Vec::new();
                        for ent in rd {
                            match ent {
                                Ok(e) => {
                                    let p = e.path();
                                    entries.push(Value::Str(p.to_string_lossy().into_owned().into()));
                                }
                                Err(e) => return Ok(err(Value::Str(format!("fs.list_dir: {e}").into()))),
                            }
                        }
                        Ok(ok(Value::List(entries.into())))
                    }
                    Err(e) => Ok(err(Value::Str(format!("fs.list_dir `{path}`: {e}").into()))),
                }
            }
            "walk" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                let mut paths: Vec<Value> = Vec::new();
                for ent in walkdir::WalkDir::new(&path) {
                    match ent {
                        Ok(e) => paths.push(Value::Str(
                            e.path().to_string_lossy().into_owned().into())),
                        Err(e) => return Ok(err(Value::Str(format!("fs.walk: {e}").into()))),
                    }
                }
                Ok(ok(Value::List(paths.into())))
            }
            "glob" => {
                let pattern = expect_str(args.first())?.to_string();
                // Glob patterns can't be path-scoped at parse time
                // (`**/*.rs` doesn't pin a directory); we filter the
                // per-result paths after expansion against
                // `--allow-fs-read`.
                let entries = match glob::glob(&pattern) {
                    Ok(e) => e,
                    Err(e) => return Ok(err(Value::Str(format!("fs.glob: {e}").into()))),
                };
                let mut paths: Vec<Value> = Vec::new();
                for ent in entries {
                    match ent {
                        Ok(p) => {
                            let s = p.to_string_lossy().into_owned();
                            if self.policy.allow_fs_read.is_empty()
                                || self.policy.allow_fs_read.iter().any(|root| p.starts_with(root))
                            {
                                paths.push(Value::Str(s.into()));
                            }
                        }
                        Err(e) => return Ok(err(Value::Str(format!("fs.glob: {e}").into()))),
                    }
                }
                Ok(ok(Value::List(paths.into())))
            }
            // Content read/write. These are the reason `[fs_read]` /
            // `[fs_write]` exist as effect names at all: the same
            // operations under `io.read` / `io.write` declare `[io]`,
            // which reads as "console" and tells a reviewer nothing
            // about filesystem reach (alpibrusl/lex-lang#882).
            "read_to_string" => {
                let path = expect_str(args.first())?.to_string();
                let resolved = self.ensure_fs_read_content_path(&path)?;
                match std::fs::read_to_string(&resolved) {
                    Ok(s) => Ok(ok(Value::Str(s.into()))),
                    Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                }
            }
            "write" => {
                let path = expect_str(args.first())?.to_string();
                let contents = expect_str(args.get(1))?.to_string();
                self.ensure_fs_write_content_path(&path)?;
                match std::fs::write(&path, contents) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                }
            }
            // Append, so a log does not have to be rewritten to grow.
            //
            // Opened with `append(true).create(true)`, so appending to a path
            // that does not exist yet behaves like `write` rather than
            // failing — a log's first line should not be a special case.
            //
            // ATOMICITY. One call issues one `write(2)` to an O_APPEND
            // descriptor. On a local filesystem the kernel makes the seek and
            // the write atomic, so concurrent appenders cannot interleave
            // within a single call for payloads up to the platform's
            // guarantee (PIPE_BUF, 4 KiB on Linux). A larger payload may be
            // split, and NFS does not guarantee it at all. Callers that need
            // a record to arrive whole should keep records small — which a
            // line-oriented log does anyway — and callers that need more
            // should be using a database.
            "append" => {
                let path = expect_str(args.first())?.to_string();
                let contents = expect_str(args.get(1))?.to_string();
                self.ensure_fs_write_content_path(&path)?;
                match std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path)
                {
                    Ok(mut f) => {
                        use std::io::Write as _;
                        match f.write_all(contents.as_bytes()) {
                            Ok(_) => Ok(ok(Value::Unit)),
                            Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                        }
                    }
                    Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                }
            }
            "mkdir_p" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_write_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::create_dir_all(&path) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.mkdir_p `{path}`: {e}").into()))),
                }
            }
            "remove" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_write_path(&path) {
                    return Ok(err(Value::Str(e.into())));
                }
                let p = std::path::Path::new(&path);
                let result = if p.is_dir() {
                    std::fs::remove_dir_all(p)
                } else {
                    std::fs::remove_file(p)
                };
                match result {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.remove `{path}`: {e}").into()))),
                }
            }
            "copy" => {
                let src = expect_str(args.first())?.to_string();
                let dst = expect_str(args.get(1))?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&src) {
                    return Ok(err(Value::Str(e.into())));
                }
                if let Err(e) = self.ensure_fs_write_path(&dst) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::copy(&src, &dst) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.copy {src} -> {dst}: {e}").into()))),
                }
            }
            other => Err(format!("unsupported fs.{other}")),
        }
    }
}

impl DefaultHandler {
    /// Path scope for walk-style operations. `[fs_walk]` reuses the
    /// `--allow-fs-read` allowlist — listing a directory is an
    /// information disclosure on the same path tree as reading file
    /// content, so the same scope applies. Empty allowlist = any path.
    pub(super) fn ensure_fs_walk_path(&self, path: &str) -> Result<(), String> {
        if self.policy.allow_fs_read.is_empty() {
            return Ok(());
        }
        let p = std::path::Path::new(path);
        if self.policy.allow_fs_read.iter().any(|a| p.starts_with(a)) {
            Ok(())
        } else {
            Err(format!("fs path `{path}` outside --allow-fs-read"))
        }
    }
}

impl DefaultHandler {
    /// Path scope for mutating operations. `[fs_write]` uses the
    /// existing `--allow-fs-write` allowlist.
    pub(super) fn ensure_fs_write_path(&self, path: &str) -> Result<(), String> {
        if self.policy.allow_fs_write.is_empty() {
            return Ok(());
        }
        let p = std::path::Path::new(path);
        if self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
            Ok(())
        } else {
            Err(format!("fs path `{path}` outside --allow-fs-write"))
        }
    }
}

impl DefaultHandler {
    /// Path scope for CONTENT reads (`fs.read_to_string`, and the legacy
    /// `io.read`). Distinct from `ensure_fs_walk_path` in one way that
    /// matters: it returns the *resolved* path, because `read_root` may
    /// rebase it for tests, and the caller must read the same path the
    /// check approved.
    pub(super) fn ensure_fs_read_content_path(&self, path: &str) -> Result<PathBuf, String> {
        let resolved = self.resolve_read_path(path);
        if !self.policy.allow_fs_read.is_empty()
            && !self.policy.allow_fs_read.iter().any(|a| resolved.starts_with(a))
        {
            return Err(format!("read of `{path}` outside --allow-fs-read"));
        }
        Ok(resolved)
    }

    /// Path scope for CONTENT writes (`fs.write`, and the legacy
    /// `io.write`). Canonicalises both sides so platform path aliases
    /// (macOS `/tmp` -> `/private/tmp`) compare correctly; `canonicalize`
    /// fails on a file that does not exist yet, so a new write falls back
    /// to canonicalising the parent.
    ///
    /// NOTE: `ensure_fs_write_path` (used by mkdir_p / remove / copy) does
    /// a plain prefix comparison with no canonicalisation, so the two
    /// disagree on aliased paths. That divergence predates this function
    /// and is deliberately not changed here.
    pub(super) fn ensure_fs_write_content_path(&self, path: &str) -> Result<(), String> {
        if self.policy.allow_fs_write.is_empty() {
            return Ok(());
        }
        let raw = std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| PathBuf::from(path));
        let p = std::fs::canonicalize(&raw).unwrap_or_else(|_| {
            raw.parent()
                .and_then(|par| std::fs::canonicalize(par).ok())
                .map(|par| par.join(raw.file_name().unwrap_or_default()))
                .unwrap_or(raw)
        });
        let allowed = self.policy.allow_fs_write.iter().any(|a| {
            let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.clone());
            p.starts_with(&ca)
        });
        if allowed {
            Ok(())
        } else {
            Err(format!("write to `{path}` outside --allow-fs-write"))
        }
    }
}
