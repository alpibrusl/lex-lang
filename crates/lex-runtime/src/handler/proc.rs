//! `proc` effect: `process.*` dispatch and the LRU-bounded child-process registry behind opaque `Process` handles.

use super::*;

pub(crate) struct ProcessState {
    pub(super) child: std::process::Child,
    pub(super) stdout: Option<std::io::BufReader<std::process::ChildStdout>>,
    pub(super) stderr: Option<std::io::BufReader<std::process::ChildStderr>>,
}

/// Process-wide registry of live `process.spawn` handles. Capped at
/// [`MAX_PROCESS_HANDLES`] to bound long-running programs that spawn
/// many short-lived children: on each `spawn` past the cap, the
/// least-recently-used entry is dropped (which `Drop`s its
/// `ProcessState`, leaving the child orphaned but the registry
/// bounded). `process.wait` also drops the entry on completion since
/// the handle becomes terminal once the child exits.
///
/// Each entry is wrapped in `Arc<Mutex<ProcessState>>` so the global
/// lookup mutex is held only briefly during dispatch — once we have
/// the per-handle `Arc`, the global lock is released and the slow
/// op (`wait`, `read_*_line`) only contends on its own handle's
/// mutex. Reads on different handles no longer block each other.
pub(super) fn process_registry() -> &'static Mutex<ProcessRegistry> {
    static REGISTRY: OnceLock<Mutex<ProcessRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(ProcessRegistry::with_capacity(MAX_PROCESS_HANDLES)))
}

pub(super) const MAX_PROCESS_HANDLES: usize = 256;

pub(super) type SharedProcessState = Arc<Mutex<ProcessState>>;

pub(crate) struct ProcessRegistry {
    pub(super) entries: indexmap::IndexMap<u64, SharedProcessState>,
    pub(super) cap: usize,
}

impl ProcessRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    /// Insert a freshly-spawned child. If at cap, evict the LRU entry
    /// first; the dropped `ProcessState`'s child stays alive (orphaned)
    /// but its file descriptors are released.
    pub(crate) fn insert(&mut self, handle: u64, state: ProcessState) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, Arc::new(Mutex::new(state)));
    }

    /// Look up a handle, marking it most-recently-used on hit. Returns
    /// a clone of the shared `Arc` — callers should release the global
    /// registry lock before locking the per-handle mutex.
    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<SharedProcessState> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle).cloned()
    }

    /// Drop the registry entry. The underlying `Arc` may outlive the
    /// removal if another op still holds it; that's intentional — the
    /// in-flight op finishes against the existing `ProcessState`, and
    /// only fresh lookups start failing.
    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.entries.len() }
}

pub(super) fn next_process_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[cfg(all(test, unix))]
mod process_registry_tests {
    use super::{ProcessRegistry, ProcessState};

    /// Spawn a trivial short-lived child for use as registry payload.
    /// `true` exits immediately — we don't actually run the child for
    /// real, we just need a valid `std::process::Child`.
    fn fresh_state() -> ProcessState {
        let child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn `true`");
        ProcessState { child, stdout: None, stderr: None }
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        assert!(r.touch_get(1).is_some());
        assert!(r.touch_get(2).is_none());
    }

    #[test]
    fn touch_get_returns_distinct_arcs_for_distinct_handles() {
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        r.insert(2, fresh_state());
        let a = r.touch_get(1).unwrap();
        let b = r.touch_get(2).unwrap();
        // Different Arcs — pointer-equality check.
        assert!(!std::sync::Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn cap_evicts_lru_on_overflow() {
        let mut r = ProcessRegistry::with_capacity(2);
        r.insert(1, fresh_state());
        r.insert(2, fresh_state());
        let _ = r.touch_get(1);
        r.insert(3, fresh_state());
        assert!(r.touch_get(1).is_some(), "1 was MRU, should survive");
        assert!(r.touch_get(2).is_none(), "2 was LRU, should be evicted");
        assert!(r.touch_get(3).is_some(), "3 just inserted, should survive");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn cap_with_no_touches_evicts_in_insertion_order() {
        let mut r = ProcessRegistry::with_capacity(2);
        r.insert(10, fresh_state());
        r.insert(20, fresh_state());
        r.insert(30, fresh_state());
        assert!(r.touch_get(10).is_none());
        assert!(r.touch_get(20).is_some());
        assert!(r.touch_get(30).is_some());
    }

    #[test]
    fn remove_drops_entry() {
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        r.remove(1);
        assert!(r.touch_get(1).is_none());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn many_inserts_stay_bounded_at_cap() {
        let cap = 8;
        let mut r = ProcessRegistry::with_capacity(cap);
        for i in 0..(cap as u64 * 3) {
            r.insert(i, fresh_state());
            assert!(r.len() <= cap);
        }
        assert_eq!(r.len(), cap);
    }

    #[test]
    fn outstanding_arc_outlives_remove() {
        // Holding the per-handle Arc while another op removes the
        // entry must not invalidate the in-flight op. Mirrors the
        // wait-completes-then-removes pattern.
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        let arc = r.touch_get(1).expect("entry exists");
        r.remove(1);
        // Registry forgot about it, but the Arc still works.
        assert!(r.touch_get(1).is_none());
        let _state = arc.lock().unwrap();
    }
}

pub(super) fn expect_process_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected ProcessHandle (Int), got {other:?}")),
        None => Err("missing ProcessHandle argument".into()),
    }
}

impl DefaultHandler {
    pub(super) fn dispatch_process(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "spawn" => {
                let cmd = expect_str(args.first())?.to_string();
                let raw_args = match args.get(1) {
                    Some(Value::List(items)) => items.clone(),
                    _ => return Err("process.spawn: args must be List[Str]".into()),
                };
                let str_args: Result<Vec<String>, String> = raw_args.iter().map(|v| match v {
                    Value::Str(s) => Ok(s.to_string()),
                    other => Err(format!("process.spawn: arg must be Str, got {other:?}")),
                }).collect();
                let str_args = str_args?;
                let opts = match args.get(2) {
                    Some(Value::Record { fields: r, .. }) => r.clone(),
                    _ => return Err("process.spawn: missing or invalid opts record".into()),
                };

                // Allow-list check, mirroring process.run below.
                if !self.policy.allow_proc.is_empty() {
                    let basename = std::path::Path::new(&cmd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&cmd);
                    if !self.policy.allow_proc.iter().any(|a| a == basename) {
                        return Ok(err(Value::Str(format!(
                            "process.spawn: `{cmd}` not in --allow-proc {:?}",
                            self.policy.allow_proc
                        ).into())));
                    }
                }

                let mut command = std::process::Command::new(&cmd);
                command.args(&str_args);
                command.stdin(std::process::Stdio::piped());
                command.stdout(std::process::Stdio::piped());
                command.stderr(std::process::Stdio::piped());

                if let Some(Value::Variant { name, args: vargs }) = opts.get("cwd") {
                    if name == "Some" {
                        if let Some(Value::Str(s)) = vargs.first() {
                            command.current_dir(s);
                        }
                    }
                }
                if let Some(Value::Map(env)) = opts.get("env") {
                    for (k, v) in env {
                        if let (lex_bytecode::MapKey::Str(ks), Value::Str(vs)) = (k, v) {
                            command.env(ks, vs);
                        }
                    }
                }

                let stdin_payload: Option<Vec<u8>> = match opts.get("stdin") {
                    Some(Value::Variant { name, args: vargs }) if name == "Some" => {
                        match vargs.first() {
                            Some(Value::Bytes(b)) => Some(b.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                };

                let mut child = match command.spawn() {
                    Ok(c) => c,
                    Err(e) => return Ok(err(Value::Str(format!("process.spawn `{cmd}`: {e}").into()))),
                };

                if let Some(payload) = stdin_payload {
                    if let Some(mut stdin) = child.stdin.take() {
                        use std::io::Write;
                        let _ = stdin.write_all(&payload);
                        // Drop closes stdin; the child sees EOF.
                    }
                }

                let stdout = child.stdout.take().map(std::io::BufReader::new);
                let stderr = child.stderr.take().map(std::io::BufReader::new);
                let handle = next_process_handle();
                process_registry().lock().unwrap().insert(handle, ProcessState {
                    child,
                    stdout,
                    stderr,
                });
                Ok(ok(Value::Int(handle as i64)))
            }
            "read_stdout_line" => Self::read_line_op(args, true),
            "read_stderr_line" => Self::read_line_op(args, false),
            "wait" => {
                let h = expect_process_handle(args.first())?;
                // Look up the per-handle Arc, then release the global
                // lock before the (slow) wait so unrelated handles
                // can dispatch concurrently.
                let arc = process_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "process.wait: closed or unknown ProcessHandle".to_string())?;
                let status = {
                    let mut state = arc.lock().unwrap();
                    state.child.wait().map_err(|e| format!("process.wait: {e}"))?
                };
                // Wait completion makes the handle terminal; drop it
                // from the registry so the cap doesn't fill up with
                // exited children.
                process_registry().lock().unwrap().remove(h);
                let mut rec = indexmap::IndexMap::new();
                rec.insert("code".into(), Value::Int(status.code().unwrap_or(-1) as i64));
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    rec.insert("signaled".into(), Value::Bool(status.signal().is_some()));
                }
                #[cfg(not(unix))]
                {
                    rec.insert("signaled".into(), Value::Bool(false));
                }
                Ok(Value::record_dynamic(rec))
            }
            "kill" => {
                let h = expect_process_handle(args.first())?;
                let _signal = expect_str(args.get(1))?;
                let arc = process_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "process.kill: closed or unknown ProcessHandle".to_string())?;
                let mut state = arc.lock().unwrap();
                // Cross-platform: only `kill` (SIGKILL-equivalent on
                // Windows). Signal-name dispatch is a v1.5 follow-up.
                match state.child.kill() {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("process.kill: {e}").into()))),
                }
            }
            "run" => {
                let cmd = expect_str(args.first())?.to_string();
                let raw_args = match args.get(1) {
                    Some(Value::List(items)) => items.clone(),
                    _ => return Err("process.run: args must be List[Str]".into()),
                };
                let str_args: Result<Vec<String>, String> = raw_args.iter().map(|v| match v {
                    Value::Str(s) => Ok(s.to_string()),
                    other => Err(format!("process.run: arg must be Str, got {other:?}")),
                }).collect();
                let str_args = str_args?;
                if !self.policy.allow_proc.is_empty() {
                    let basename = std::path::Path::new(&cmd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&cmd);
                    if !self.policy.allow_proc.iter().any(|a| a == basename) {
                        return Ok(err(Value::Str(format!(
                            "process.run: `{cmd}` not in --allow-proc {:?}",
                            self.policy.allow_proc
                        ).into())));
                    }
                }
                match std::process::Command::new(&cmd).args(&str_args).output() {
                    Ok(o) => {
                        let mut rec = indexmap::IndexMap::new();
                        rec.insert("stdout".into(), Value::Str(
                            String::from_utf8_lossy(&o.stdout).into_owned().into()));
                        rec.insert("stderr".into(), Value::Str(
                            String::from_utf8_lossy(&o.stderr).into_owned().into()));
                        rec.insert("exit_code".into(), Value::Int(
                            o.status.code().unwrap_or(-1) as i64));
                        Ok(ok(Value::record_dynamic(rec)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("process.run `{cmd}`: {e}").into()))),
                }
            }
            other => Err(format!("unsupported process.{other}")),
        }
    }
}

impl DefaultHandler {
    /// Read one line from the child's stdout (`is_stdout = true`) or
    /// stderr. Returns `None` (Lex `Option`) at EOF; subsequent calls
    /// keep returning `None`. Holds only the per-handle mutex during
    /// the (potentially blocking) read, so reads on one handle don't
    /// block reads/waits on a different handle.
    pub(super) fn read_line_op(args: Vec<Value>, is_stdout: bool) -> Result<Value, String> {
        let h = expect_process_handle(args.first())?;
        let arc = process_registry().lock().unwrap()
            .touch_get(h)
            .ok_or_else(|| format!(
                "process.read_{}_line: closed or unknown ProcessHandle",
                if is_stdout { "stdout" } else { "stderr" }))?;
        let mut state = arc.lock().unwrap();
        let reader_opt = if is_stdout {
            state.stdout.as_mut().map(|r| -> &mut dyn std::io::BufRead { r })
        } else {
            state.stderr.as_mut().map(|r| -> &mut dyn std::io::BufRead { r })
        };
        let reader = match reader_opt {
            Some(r) => r,
            None => return Ok(none()),
        };
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => Ok(none()),
            Ok(_) => {
                if line.ends_with('\n') { line.pop(); }
                if line.ends_with('\r') { line.pop(); }
                Ok(some(Value::Str(line.into())))
            }
            Err(e) => Err(format!("process.read_*_line: {e}")),
        }
    }
}
