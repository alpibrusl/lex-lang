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
