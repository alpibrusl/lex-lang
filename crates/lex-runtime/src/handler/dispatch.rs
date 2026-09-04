//! `EffectHandler` for `DefaultHandler`: the `(kind, op)` router every runtime effect passes through, plus request-scope, budget and worker hooks. Family-specific helpers live in the sibling modules.

use super::*;

impl EffectHandler for DefaultHandler {
    /// Push a fresh per-request arena onto the stack (#463
    /// scaffolding). Returns the scope id; pair with
    /// `exit_request_scope(id)` to drop it.
    fn enter_request_scope(&mut self) -> u64 {
        let id = self.next_scope_id;
        self.next_scope_id = self.next_scope_id.wrapping_add(1);
        self.arena_stack.push((id, crate::arena::Arena::new()));
        id
    }

    /// Drop the arena associated with `scope_id`. Mismatched pairs
    /// (exit called with a scope id we don't recognize, or out-of-
    /// order exit) are tolerated as no-ops rather than panicking —
    /// runtime layer should pair them strictly but a stray exit
    /// shouldn't crash a live server.
    fn exit_request_scope(&mut self, scope_id: u64) {
        if let Some(pos) = self.arena_stack.iter().position(|(id, _)| *id == scope_id) {
            // Drop this entry and any later entries that escaped
            // pairing (out-of-order exit). Order matters: pop in
            // reverse so the most recent arena drops first, then
            // its predecessor, etc.
            self.arena_stack.truncate(pos);
        }
    }

    /// Per-call budget enforcement (#225). VM calls this before
    /// invoking any function whose signature declares `[budget(N)]`.
    /// The cost N is deducted atomically from the shared pool;
    /// returning `Err` aborts the call before any frame is pushed.
    fn note_call_budget(&mut self, cost: u64) -> Result<(), String> {
        // Skip the work entirely when no ceiling is configured —
        // the pool is `u64::MAX` and would never trip.
        let Some(ceiling) = self.budget_ceiling else { return Ok(()); };
        // Compare-and-swap: speculatively subtract; if we'd
        // underflow, return BudgetExceeded without mutating.
        // Use SeqCst because parallel branches may race here and
        // the relative ordering of "used so far" vs. "this call's
        // cost" needs to be deterministic across threads.
        loop {
            let cur = self.budget_remaining.load(Ordering::SeqCst);
            if cost > cur {
                let used = ceiling.saturating_sub(cur);
                return Err(format!(
                    "budget exceeded: requested {cost}, used so far {used}, ceiling {ceiling}"));
            }
            let next = cur - cost;
            // Conservative accounting: if the CAS races and loses,
            // re-read and try again. No refund-on-failure path.
            if self.budget_remaining.compare_exchange(cur, next,
                Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return Ok(());
            }
        }
    }

    fn dispatch(&mut self, kind: &str, op: &str, args: Vec<Value>) -> Result<Value, String> {
        // Pure stdlib builtins (str, list, json, ...) bypass the policy
        // gate — they have no observable side effects and aren't tracked
        // by the type system as effects.
        if is_pure_call(kind, op) {
            return call_pure_builtin(kind, op, args);
        }
        // `std.fs` ops use the fine-grained `[fs_walk]` and `[fs_write]`
        // effect kinds (distinct from the module name `fs`); the
        // policy check uses the per-op kind, not the module's.
        if kind == "process" {
            self.ensure_kind_allowed("proc")?;
            return self.dispatch_process(op, args);
        }
        if kind == "approval" {
            self.ensure_kind_allowed("approval")?;
            return self.dispatch_approval(op, args);
        }
        if kind == "log" {
            // Emit ops are [log]; config ops are [io] (set_sink also
            // [fs_write]). The dispatch picks the right kind per op.
            let effect_kind = match op {
                "debug" | "info" | "warn" | "error" => "log",
                "set_level" | "set_format" => "io",
                "set_sink" => {
                    self.ensure_kind_allowed("io")?;
                    self.ensure_kind_allowed("fs_write")?;
                    return self.dispatch_log(op, args);
                }
                other => return Err(format!("unsupported log.{other}")),
            };
            self.ensure_kind_allowed(effect_kind)?;
            return self.dispatch_log(op, args);
        }
        if kind == "fs" {
            let effect_kind = match op {
                "exists" | "is_file" | "is_dir" | "stat"
                | "list_dir" | "walk" | "glob" => "fs_walk",
                "mkdir_p" | "remove" => "fs_write",
                "copy" => {
                    self.ensure_kind_allowed("fs_walk")?;
                    self.ensure_kind_allowed("fs_write")?;
                    return self.dispatch_fs(op, args);
                }
                other => return Err(format!("unsupported fs.{other}")),
            };
            self.ensure_kind_allowed(effect_kind)?;
            return self.dispatch_fs(op, args);
        }
        // `crypto.random` is the lone effectful op in `std.crypto`. Its
        // declared effect kind is `random` (fine-grained on purpose so
        // `lex audit --effect random` flags every token-generating
        // call), distinct from the `crypto` module name.
        // datetime.now is the only effectful op in std.datetime;
        // declared kind is `time`, matching the existing `time.now`.
        if kind == "datetime" && op == "now" {
            self.ensure_kind_allowed("time")?;
            // LEX_TEST_NOW (Unix seconds) pins the clock for deterministic tests (#350).
            if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                if let Ok(secs) = s.trim().parse::<i64>() {
                    return Ok(Value::Int(secs.saturating_mul(1_000_000_000)));
                }
            }
            let now = chrono::Utc::now();
            let nanos = now.timestamp_nanos_opt().unwrap_or(i64::MAX);
            return Ok(Value::Int(nanos));
        }
        if kind == "crypto" && op == "random" {
            self.ensure_kind_allowed("random")?;
            let n = expect_int(args.first())?;
            if !(0..=1_048_576).contains(&n) {
                return Err("crypto.random: n must be in 0..=1048576".into());
            }
            use rand::{rngs::SysRng, TryRng};
            let mut buf = vec![0u8; n as usize];
            SysRng.try_fill_bytes(&mut buf)
                .map_err(|e| format!("crypto.random: OS RNG: {e}"))?;
            return Ok(Value::Bytes(buf));
        }
        // crypto.random_str_hex(n) — N random bytes rendered as 2N
        // lowercase hex chars (#382). The most common token-mint
        // pattern (session ids, OAuth `state`, CSRF, request ids).
        // Same `[random]` gate as `crypto.random`.
        if kind == "crypto" && op == "random_str_hex" {
            self.ensure_kind_allowed("random")?;
            let n = expect_int(args.first())?;
            if !(0..=1_048_576).contains(&n) {
                return Err("crypto.random_str_hex: n must be in 0..=1048576".into());
            }
            use rand::{rngs::SysRng, TryRng};
            let mut buf = vec![0u8; n as usize];
            SysRng.try_fill_bytes(&mut buf)
                .map_err(|e| format!("crypto.random_str_hex: OS RNG: {e}"))?;
            return Ok(Value::Str(hex::encode(&buf).into()));
        }
        // crypto.p256_generate() — mint a fresh P-256 (ES256) secret
        // key from the OS RNG (#651). Returns the 32-byte scalar as
        // `Ok(Bytes)`. Same `[random]` gate as `crypto.random`: key
        // minting stays visible to `lex audit --effect random`.
        //
        // We sample 32 bytes and let `SigningKey::from_slice` reject
        // the (vanishingly rare, ~2^-32) out-of-range scalar rather
        // than pulling in p256's own `rand_core` — that crate is on a
        // different `rand_core` major than the workspace `rand`, so
        // bridging RNG traits here would mean an extra dependency for
        // no behavioural gain. Retry a handful of times so a one-in-
        // four-billion miss never surfaces as a spurious `Err`.
        if kind == "crypto" && op == "p256_generate" {
            self.ensure_kind_allowed("random")?;
            use p256::ecdsa::SigningKey;
            use rand::{rngs::SysRng, TryRng};
            for _ in 0..16 {
                let mut buf = [0u8; 32];
                SysRng.try_fill_bytes(&mut buf)
                    .map_err(|e| format!("crypto.p256_generate: OS RNG: {e}"))?;
                if let Ok(sk) = SigningKey::from_slice(&buf) {
                    return Ok(ok(Value::Bytes(sk.to_bytes().to_vec())));
                }
            }
            return Ok(err(Value::Str(
                "crypto.p256_generate: failed to sample a valid scalar".into())));
        }
        // crypto.secp256k1_generate() — mint a fresh secp256k1 secret key
        // from the OS RNG (#655) for EVM / EIP-712 / x402 signing. Returns
        // the 32-byte scalar as `Ok(Bytes)`. Same `[random]` gate and
        // sample-and-reject loop as `p256_generate` (the curve order is
        // close enough to 2^256 that a miss is ~2^-128, but the loop
        // keeps the contract identical).
        if kind == "crypto" && op == "secp256k1_generate" {
            self.ensure_kind_allowed("random")?;
            use k256::ecdsa::SigningKey;
            use rand::{rngs::SysRng, TryRng};
            for _ in 0..16 {
                let mut buf = [0u8; 32];
                SysRng.try_fill_bytes(&mut buf)
                    .map_err(|e| format!("crypto.secp256k1_generate: OS RNG: {e}"))?;
                if let Ok(sk) = SigningKey::from_slice(&buf) {
                    return Ok(ok(Value::Bytes(sk.to_bytes().to_vec())));
                }
            }
            return Ok(err(Value::Str(
                "crypto.secp256k1_generate: failed to sample a valid scalar".into())));
        }
        // `std.http` wire ops (send/get/post) gate on the `net`
        // effect kind, not the module name. This matches the
        // declared signature (`http.get :: Str -> [net] ...`) and
        // keeps `--allow-effects net` doing the obvious thing for
        // both `net.*` and `http.*` callers.
        // `std.agent` (#184): the four runtime effects added for
        // agent-style programs (`llm_local`, `llm_cloud`, `a2a`,
        // `mcp`). The handlers are stubs — they enforce the
        // declared-effect gate, return a sentinel `Ok` so traces
        // record the call, and defer the real wire formats to
        // downstream crates (`soft-agent` for `llm_*` and `a2a`)
        // and #185 (MCP client wrapper).
        if kind == "agent" {
            let effect_kind = match op {
                "local_complete" => "llm_local",
                "cloud_complete" => "llm_cloud",
                "cloud_stream"   => "llm_cloud",
                "send_a2a"       => "a2a",
                "call_mcp"       => "mcp",
                other => return Err(format!("unsupported agent.{other}")),
            };
            self.ensure_kind_allowed(effect_kind)?;
            // `call_mcp` runs through the LRU client cache
            // (#197). `local_complete` / `cloud_complete` hit
            // Ollama / OpenAI via env-var-driven configuration
            // (#196); custom backends override at the
            // EffectHandler layer rather than via a config file.
            // `send_a2a` keeps its stub — that wire format
            // lives in downstream `soft-a2a`.
            return match op {
                "call_mcp"       => Ok(self.dispatch_call_mcp(args)),
                "local_complete" => Ok(dispatch_llm_local(args)),
                "cloud_complete" => Ok(dispatch_llm_cloud(args)),
                "cloud_stream"   => Ok(self.dispatch_cloud_stream(args)),
                _ => Ok(ok(Value::Str(format!("<{effect_kind} stub>").into()))),
            };
        }
        if kind == "stream" {
            // #305 slice 3: consumer-side stream operations. Each
            // op resolves the opaque handle in the parent handler's
            // stream registry and pulls one or all items. The
            // `stream` effect must be granted by policy; default
            // policies for agent runs grant it alongside the
            // producer effect (e.g. `llm_cloud`).
            self.ensure_kind_allowed("stream")?;
            return match op {
                "next"    => Ok(self.dispatch_stream_next(args)),
                "collect" => Ok(self.dispatch_stream_collect(args)),
                other => Err(format!("unsupported stream.{other}")),
            };
        }
        if kind == "http" && matches!(op, "send" | "get" | "post" | "stream_lines") {
            self.ensure_kind_allowed("net")?;
            return match op {
                "send" => {
                    let req = expect_record(args.first())?;
                    Ok(http_send_record(self, req))
                }
                "get" => {
                    let url = expect_str(args.first())?.to_string();
                    self.ensure_host_allowed(&url)?;
                    Ok(http_send_simple("GET", &url, None, "", None))
                }
                "post" => {
                    let url = expect_str(args.first())?.to_string();
                    let body = expect_bytes(args.get(1))?.clone();
                    let content_type = expect_str(args.get(2))?.to_string();
                    self.ensure_host_allowed(&url)?;
                    Ok(http_send_simple("POST", &url, Some(body), &content_type, None))
                }
                "stream_lines" => {
                    let url = expect_str(args.first())?.to_string();
                    let headers_val = args.get(1).cloned().unwrap_or(Value::Map(Default::default()));
                    let body = expect_str(args.get(2))?.to_string();
                    self.ensure_host_allowed(&url)?;
                    Ok(http_stream_lines_impl(self, &url, &headers_val, &body))
                }
                _ => unreachable!(),
            };
        }
        // `arrow.read_csv` declares `[fs_read]`, not `[arrow]` — its effect
        // string in the type system is `fs_read`. Intercept before the
        // generic `ensure_kind_allowed(kind)` below so the policy check
        // looks at `fs_read` rather than `arrow`. Same pattern as
        // `http.{send,get,post}` mapping to `[net]` above.
        if kind == "arrow" && op == "read_csv" {
            self.ensure_kind_allowed("fs_read")?;
            let path = expect_str(args.first())?.to_string();
            let resolved = self.resolve_read_path(&path);
            if !self.policy.allow_fs_read.is_empty()
                && !self.policy.allow_fs_read.iter().any(|a| resolved.starts_with(a))
            {
                return Err(format!("arrow.read_csv: `{path}` outside --allow-fs-read"));
            }
            return match crate::arrow::read_csv_at(&resolved) {
                Ok(v)  => Ok(ok(v)),
                Err(e) => Ok(err(Value::Str(e.into()))),
            };
        }
        // `arrow.read_parquet` and `arrow.read_parquet_cols` are the
        // Parquet siblings of `read_csv`. Same `[fs_read]` effect, same
        // path-scope check. `_cols` takes an extra `List[Str]` argument.
        if kind == "arrow" && (op == "read_parquet" || op == "read_parquet_cols") {
            self.ensure_kind_allowed("fs_read")?;
            let path = expect_str(args.first())?.to_string();
            let resolved = self.resolve_read_path(&path);
            if !self.policy.allow_fs_read.is_empty()
                && !self.policy.allow_fs_read.iter().any(|a| resolved.starts_with(a))
            {
                return Err(format!("arrow.{op}: `{path}` outside --allow-fs-read"));
            }
            let r = if op == "read_parquet" {
                crate::arrow::read_parquet_at(&resolved)
            } else {
                let cols = match args.get(1) {
                    Some(Value::List(items)) => {
                        let mut out = Vec::with_capacity(items.len());
                        for v in items.iter() {
                            match v {
                                Value::Str(s) => out.push(s.to_string()),
                                other => return Err(format!(
                                    "arrow.read_parquet_cols: column name not Str: {other:?}")),
                            }
                        }
                        out
                    }
                    other => return Err(format!(
                        "arrow.read_parquet_cols: expected List[Str], got {other:?}")),
                };
                crate::arrow::read_parquet_cols_at(&resolved, &cols)
            };
            return match r {
                Ok(v) => Ok(ok(v)),
                Err(e) => Ok(err(Value::Str(e.into()))),
            };
        }
        // `arrow.write_parquet` and `arrow.write_csv` declare `[fs_write]`.
        // Path scope uses `--allow-fs-write` (symmetric with `io.write`).
        if kind == "arrow" && (op == "write_parquet" || op == "write_csv") {
            self.ensure_kind_allowed("fs_write")?;
            let table_v = args.first().cloned().unwrap_or(Value::Unit);
            let rb = match &table_v {
                Value::ArrowTable(t) => Arc::clone(t),
                other => return Err(format!("arrow.{op}: first arg must be arrow.Table, got {other:?}")),
            };
            let path = expect_str(args.get(1))?.to_string();
            if let Err(e) = self.ensure_fs_write_path(&path) {
                return Ok(err(Value::Str(format!("arrow.{op}: {e}").into())));
            }
            let r = if op == "write_parquet" {
                crate::arrow::write_parquet_at(&rb, std::path::Path::new(&path))
            } else {
                crate::arrow::write_csv_at(&rb, std::path::Path::new(&path))
            };
            return match r {
                Ok(_)  => Ok(ok(Value::Unit)),
                Err(e) => Ok(err(Value::Str(e.into()))),
            };
        }
        // `net.default_opts()` is a pure record constructor — typed
        // with `EffectSet::empty()` in builtins.rs. Bypass the generic
        // `ensure_kind_allowed("net")` gate so callers don't need to
        // declare `[net]` just to build a ServeOpts literal default.
        if kind == "net" && op == "default_opts" {
            return Ok(ServeOpts::lex_defaults().to_value());
        }
        // `tls.*` (#496) — TlsConfig constructors map to different
        // effect kinds than the namespace name suggests:
        //   `tls.from_pem_files` :: [fs_read]   (reads cert + key PEM)
        //   `tls.self_signed`    :: pure        (rcgen, in-memory)
        // Intercept before the generic `ensure_kind_allowed("tls")`
        // gate so policy can check the *real* effect. Same pattern
        // as the `http.{send,get,post}` arms above.
        if kind == "tls" {
            return match op {
                "from_pem_files" => {
                    self.ensure_kind_allowed("fs_read")?;
                    dispatch_tls_from_pem_files(self, args)
                }
                "self_signed" => dispatch_tls_self_signed(args),
                other => Err(format!("unsupported tls.{other}")),
            };
        }
        // `std.redis` ops all carry `[net]` in their declared effect sets,
        // not `[redis]`. Gate on `net` here and skip the generic kind-check
        // below, matching the `std.http` precedent.
        if kind == "redis" {
            self.ensure_kind_allowed("net")?;
        } else if kind == "rand" {
            // `std.rand.int_in` draws from the OS RNG → `[random]` effect,
            // the same gate as `crypto.random` (#677). No separate `rand`
            // effect grant exists.
            self.ensure_kind_allowed("random")?;
        } else {
            self.ensure_kind_allowed(kind)?;
        }
        match (kind, op) {
            ("io", "print") => {
                let line = expect_str(args.first())?;
                self.sink.print_line(line);
                Ok(Value::Unit)
            }
            ("io", "read") => {
                let path = expect_str(args.first())?.to_string();
                let resolved = self.resolve_read_path(&path);
                // Honor read-allowlist if any. Symmetric with io.write.
                // The path argument is checked as-given (resolved-against-
                // read_root for tests); a tool granted [io] cannot escape
                // the configured prefix even though the effect itself is
                // permitted. This is the per-path scope the bench's case
                // #6 ("[io] granted, body reads /etc/passwd") needed.
                if !self.policy.allow_fs_read.is_empty()
                    && !self.policy.allow_fs_read.iter().any(|a| resolved.starts_with(a))
                {
                    return Err(format!("read of `{path}` outside --allow-fs-read"));
                }
                match std::fs::read_to_string(&resolved) {
                    Ok(s) => Ok(ok(Value::Str(s.into()))),
                    Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                }
            }
            ("io", "readline") => {
                use std::io::BufRead;
                let stdin = std::io::stdin();
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) => Ok(Value::Variant { name: "None".into(), args: vec![] }),
                    Ok(_) => {
                        if line.ends_with('\n') { line.pop(); }
                        if line.ends_with('\r') { line.pop(); }
                        Ok(Value::Variant { name: "Some".into(), args: vec![Value::Str(line.into())] })
                    }
                    Err(_) => Ok(Value::Variant { name: "None".into(), args: vec![] }),
                }
            }
            ("io", "argv") => {
                let list: Vec<Value> = self.program_args.iter()
                    .map(|s| Value::Str(s.as_str().into()))
                    .collect();
                Ok(Value::List(list.into()))
            }
            ("io", "write") => {
                let path = expect_str(args.first())?.to_string();
                let contents = expect_str(args.get(1))?.to_string();
                // Honor write-allowlist if any.
                // Canonicalize both sides so macOS /tmp → /private/tmp symlinks
                // and other platform-specific path aliases compare correctly.
                if !self.policy.allow_fs_write.is_empty() {
                    let raw = std::env::current_dir()
                        .map(|cwd| cwd.join(&path))
                        .unwrap_or_else(|_| std::path::PathBuf::from(&path));
                    // canonicalize fails if the file doesn't exist yet (new writes).
                    // Fall back to canonicalizing the parent so macOS /tmp → /private/tmp
                    // symlinks still compare correctly against the allowlist.
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
                    if !allowed {
                        return Err(format!("write to `{path}` outside --allow-fs-write"));
                    }
                }
                match std::fs::write(&path, contents) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("{e}").into()))),
                }
            }
            ("time", "now") => {
                // LEX_TEST_NOW (Unix seconds) pins for deterministic tests.
                if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                    if let Ok(secs) = s.trim().parse::<i64>() {
                        return Ok(Value::Int(secs));
                    }
                }
                let secs = SystemTime::now().duration_since(UNIX_EPOCH)
                    .map_err(|e| format!("time: {e}"))?.as_secs();
                Ok(Value::Int(secs as i64))
            }
            ("time", "now_ms") => {
                // Unix epoch in milliseconds (#378). `LEX_TEST_NOW` is
                // documented in seconds, so we lift it to ms by *1000
                // to keep the pinning story uniform across `time.now`
                // and `time.now_ms`.
                if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                    if let Ok(secs) = s.trim().parse::<i64>() {
                        return Ok(Value::Int(secs.saturating_mul(1000)));
                    }
                }
                let ms = SystemTime::now().duration_since(UNIX_EPOCH)
                    .map_err(|e| format!("time: {e}"))?.as_millis();
                Ok(Value::Int(ms as i64))
            }
            ("time", "now_str") => {
                // ISO-8601 / RFC 3339 in UTC (#378). Format mirrors
                // `chrono::Utc::now().to_rfc3339()` already used
                // elsewhere in the handler.
                if let Ok(s) = std::env::var("LEX_TEST_NOW") {
                    if let Ok(secs) = s.trim().parse::<i64>() {
                        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
                            .unwrap_or_else(chrono::Utc::now);
                        return Ok(Value::Str(dt.to_rfc3339().into()));
                    }
                }
                Ok(Value::Str(chrono::Utc::now().to_rfc3339().into()))
            }
            ("time", "mono_ns") => {
                // Monotonic clock relative to process start. Cached
                // `Instant::now()` anchor so successive `mono_ns`
                // calls return strictly non-decreasing values without
                // depending on the wall clock. Not affected by
                // `LEX_TEST_NOW` — pinning a monotonic clock would
                // defeat its purpose; tests needing a fake monotonic
                // clock should swap in their own `EffectHandler`.
                static MONO_START: OnceLock<std::time::Instant> = OnceLock::new();
                let start = MONO_START.get_or_init(std::time::Instant::now);
                let dur = std::time::Instant::now().duration_since(*start);
                Ok(Value::Int(dur.as_nanos() as i64))
            }
            ("time", "sleep_ms") => {
                // Block the current thread for `n` ms (#226). Used
                // by `flow.retry_with_backoff`'s exponential delay.
                // Negative or zero is a no-op. Bounded at 60s in the
                // runtime to avoid pathological agent-emitted loops
                // wedging the host — anything legitimate beyond
                // that should use process-level scheduling, not a
                // blocking sleep.
                let n = expect_int(args.first())?;
                if n > 0 {
                    let ms = (n as u64).min(60_000);
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                Ok(Value::Unit)
            }
            ("time", "sleep") => {
                // Duration-typed sleep (#445). Duration values are
                // backed by `Int` nanoseconds at runtime (see the
                // `datetime.duration_*` constructors). Same 60s cap
                // as `sleep_ms` — kept consistent so all blocking
                // sleeps share one ceiling.
                let nanos = expect_int(args.first())?;
                if nanos > 0 {
                    let bounded_nanos = (nanos as u64).min(60_000 * 1_000_000);
                    std::thread::sleep(std::time::Duration::from_nanos(bounded_nanos));
                }
                Ok(Value::Unit)
            }
            ("rand", "int_in") => {
                // Honest uniform draw in [lo, hi] inclusive from the OS RNG
                // (#677), replacing the old deterministic midpoint stub.
                // Same entropy source as `crypto.random`; gated `[random]`.
                let lo = expect_int(args.first())?;
                let hi = expect_int(args.get(1))?;
                if hi < lo {
                    return Err(format!("rand.int_in: empty range [{lo}, {hi}]"));
                }
                use rand::{rngs::SysRng, TryRng};
                // span fits in u128 even for the full i64 range; bias from
                // the modulo over a 128-bit draw is < 2^-64 (negligible).
                let span = (hi as i128 - lo as i128 + 1) as u128;
                let mut buf = [0u8; 16];
                SysRng.try_fill_bytes(&mut buf)
                    .map_err(|e| format!("rand.int_in: OS RNG: {e}"))?;
                let draw = (u128::from_le_bytes(buf) % span) as i128;
                Ok(Value::Int((lo as i128 + draw) as i64))
            }
            // `env.get` returns `Option[Str]` — `None` for unset vars.
            // Per-var scoping (`[env(NAME)]`) arrives with #207's
            // per-capability effect parameterization; today the flat
            // `[env]` grants access to the entire process environment.
            ("env", "get") => {
                let name = expect_str(args.first())?;
                Ok(match std::env::var(name) {
                    Ok(v) => Value::Variant {
                        name: "Some".into(),
                        args: vec![Value::Str(v.into())],
                    },
                    Err(_) => Value::Variant { name: "None".into(), args: Vec::new() },
                })
            }
            ("budget", _) => {
                // Budget calls are nominally tracked here; budget itself is
                // enforced statically in `policy::check_program`.
                Ok(Value::Unit)
            }
            ("net", "get") => {
                let url = expect_str(args.first())?.to_string();
                self.ensure_host_allowed(&url)?;
                Ok(http_request("GET", &url, None))
            }
            ("net", "post") => {
                let url = expect_str(args.first())?.to_string();
                let body = expect_str(args.get(1))?.to_string();
                self.ensure_host_allowed(&url)?;
                Ok(http_request("POST", &url, Some(&body)))
            }
            // ── UDP datagrams (#760) ──────────────────────────────
            ("net", "udp_open") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.udp_open(port): port must be Int 0..=65535".into()),
                };
                let mut reg = udp_registry().lock().unwrap();
                if reg.len() >= MAX_UDP_HANDLES {
                    return Ok(err(Value::Str(format!(
                        "net.udp_open: too many open sockets ({MAX_UDP_HANDLES}); \
                         close them with net.udp_close"
                    ).into())));
                }
                match std::net::UdpSocket::bind(("0.0.0.0", port)) {
                    Ok(sock) => {
                        let handle = next_udp_handle();
                        reg.insert(handle, sock);
                        Ok(ok(Value::Int(handle as i64)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("net.udp_open: {e}").into()))),
                }
            }
            ("net", "udp_close") => {
                let handle = expect_int(args.first()).map_err(|e| format!("net.udp_close(sock): {e}"))?;
                // Dropping the socket closes it. Idempotent on purpose: a
                // double close is a caller being careful, not an error.
                udp_registry().lock().unwrap().remove(&(handle as u64));
                Ok(ok(Value::Unit))
            }
            ("net", "udp_send") => {
                let handle = expect_int(args.first()).map_err(|e| format!("net.udp_send(sock, ...): {e}"))?;
                let host = expect_str(args.get(1))?.to_string();
                let port = match args.get(2) {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.udp_send(sock, host, port, data): port must be Int 0..=65535".into()),
                };
                let data = match args.get(3) {
                    Some(Value::Bytes(b)) => b.clone(),
                    _ => return Err("net.udp_send(sock, host, port, data): data must be Bytes".into()),
                };
                // The same gate `net.get` applies to a URL's host, applied
                // to the datagram's destination. Without this, `udp_send`
                // would be a way around the only network policy this
                // module has. Broadcast and multicast addresses are not
                // special-cased: they must be allowlisted like anything
                // else, which is the point.
                if let Err(e) = self.ensure_udp_dest_allowed(&host) {
                    return Ok(err(Value::Str(e.into())));
                }
                let res = with_udp(handle, "net.udp_send", |sock| {
                    sock.send_to(&data, (host.as_str(), port))
                        .map_err(|e| format!("net.udp_send: {e}"))
                });
                match res {
                    Ok(n) => Ok(ok(Value::Int(n as i64))),
                    Err(e) => Ok(err(Value::Str(e.into()))),
                }
            }
            ("net", "udp_recv") => {
                let handle = expect_int(args.first()).map_err(|e| format!("net.udp_recv(sock, ...): {e}"))?;
                let timeout_ms = expect_int(args.get(1)).map_err(|e| format!("net.udp_recv(..., timeout_ms): {e}"))?;
                if timeout_ms < 0 {
                    return Err("net.udp_recv(sock, timeout_ms): timeout_ms must be >= 0".into());
                }
                let res = with_udp(handle, "net.udp_recv", |sock| {
                    // 0 means "no timeout" to the OS, which would block
                    // this thread forever. Callers asking for 0 want a
                    // poll, so give them the shortest real timeout instead.
                    let d = std::time::Duration::from_millis(
                        if timeout_ms == 0 { 1 } else { timeout_ms as u64 });
                    sock.set_read_timeout(Some(d))
                        .map_err(|e| format!("net.udp_recv: setting timeout: {e}"))?;
                    // 65507 is the largest payload IPv4/UDP can carry, so
                    // this cannot truncate a legal datagram. recv_from
                    // discards any excess rather than reporting it, which
                    // would be a silent wrong answer.
                    let mut buf = vec![0u8; 65_507];
                    match sock.recv_from(&mut buf) {
                        Ok((n, addr)) => {
                            buf.truncate(n);
                            Ok(udp_datagram_value(buf, addr))
                        }
                        Err(e) if matches!(e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                            // Deliberately Err, not an empty datagram: a
                            // zero-length UDP payload is legal, and the
                            // caller must be able to tell "nothing came"
                            // from "something empty came".
                            Err(format!("net.udp_recv: timed out after {timeout_ms}ms")),
                        Err(e) => Err(format!("net.udp_recv: {e}")),
                    }
                });
                match res {
                    Ok(v) => Ok(ok(v)),
                    Err(e) => Ok(err(Value::Str(e.into()))),
                }
            }
            ("net", "udp_broadcast") => {
                let handle = expect_int(args.first()).map_err(|e| format!("net.udp_broadcast(sock, on): {e}"))?;
                let on = matches!(args.get(1), Some(Value::Bool(true)));
                let res = with_udp(handle, "net.udp_broadcast", |sock| {
                    sock.set_broadcast(on)
                        .map_err(|e| format!("net.udp_broadcast: {e}"))
                });
                match res {
                    Ok(()) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(e.into()))),
                }
            }
            ("net", "udp_join_multicast") => {
                let handle = expect_int(args.first()).map_err(|e| format!("net.udp_join_multicast(sock, group): {e}"))?;
                let group = expect_str(args.get(1))?.to_string();
                let res = with_udp(handle, "net.udp_join_multicast", |sock| {
                    let g: std::net::Ipv4Addr = group.parse().map_err(|_| format!(
                        "net.udp_join_multicast: `{group}` is not an IPv4 address"))?;
                    if !g.is_multicast() {
                        return Err(format!(
                            "net.udp_join_multicast: {g} is not a multicast address \
                             (224.0.0.0/4)"));
                    }
                    sock.join_multicast_v4(&g, &std::net::Ipv4Addr::UNSPECIFIED)
                        .map_err(|e| format!("net.udp_join_multicast: {e}"))
                });
                match res {
                    Ok(()) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(e.into()))),
                }
            }
            ("net", "serve") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve(port, handler): port must be Int 0..=65535".into()),
                };
                let handler_name = expect_str(args.get(1))?.to_string();
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http(port, handler_name, program, policy, None, ServeOpts::from_env())
            }
            ("net", "serve_fn") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_fn(port, handler): port must be Int 0..=65535".into()),
                };
                let closure = match args.into_iter().nth(1) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_fn(port, handler): handler must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_fn requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_fn(port, closure, program, policy, ServeOpts::from_env())
            }
            ("net", "serve_routed") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_routed(port, routes, fallback): port must be Int 0..=65535".into()),
                };
                let routes_val = args.get(1).cloned()
                    .ok_or_else(|| "net.serve_routed(port, routes, fallback): missing routes".to_string())?;
                let fallback = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_routed(port, routes, fallback): fallback must be a closure".into()),
                };
                let routes = decode_routes_arg(routes_val)?;
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_routed requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_routed(port, routes, fallback, program, policy, ServeOpts::from_env())
            }
            ("net", "serve_with") => {
                // serve_with(port, handler_name, opts)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_with(port, handler, opts): port must be Int 0..=65535".into()),
                };
                let handler_name = expect_str(args.get(1))?.to_string();
                let opts = decode_serve_opts(args.get(2)
                    .ok_or_else(|| "net.serve_with(port, handler, opts): missing opts".to_string())?)?;
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_with requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http(port, handler_name, program, policy, None, opts)
            }
            ("net", "serve_fn_with") => {
                // serve_fn_with(port, handler_closure, opts)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_fn_with(port, handler, opts): port must be Int 0..=65535".into()),
                };
                let opts = decode_serve_opts(args.get(2)
                    .ok_or_else(|| "net.serve_fn_with(port, handler, opts): missing opts".to_string())?)?;
                let closure = match args.into_iter().nth(1) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_fn_with(port, handler, opts): handler must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_fn_with requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_fn(port, closure, program, policy, opts)
            }
            ("net", "serve_routed_with") => {
                // serve_routed_with(port, routes, fallback, opts)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_routed_with(port, routes, fallback, opts): port must be Int 0..=65535".into()),
                };
                let routes_val = args.get(1).cloned()
                    .ok_or_else(|| "net.serve_routed_with(port, routes, fallback, opts): missing routes".to_string())?;
                let opts = decode_serve_opts(args.get(3)
                    .ok_or_else(|| "net.serve_routed_with(port, routes, fallback, opts): missing opts".to_string())?)?;
                let fallback = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_routed_with(port, routes, fallback, opts): fallback must be a closure".into()),
                };
                let routes = decode_routes_arg(routes_val)?;
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_routed_with requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http_routed(port, routes, fallback, program, policy, opts)
            }
            ("net", "serve_quic") => self.dispatch_serve_quic_named(args),
            ("net", "serve_quic_fn") => self.dispatch_serve_quic_fn(args),
            ("net", "serve_quic_routed") => self.dispatch_serve_quic_routed(args),
            ("net", "serve_tls") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_tls(port, cert, key, handler): port must be Int 0..=65535".into()),
                };
                let cert_path = expect_str(args.get(1))?.to_string();
                let key_path = expect_str(args.get(2))?.to_string();
                let handler_name = expect_str(args.get(3))?.to_string();
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_tls requires a Program reference".to_string())?;
                let policy = self.policy.clone();
                let cert = std::fs::read(&cert_path)
                    .map_err(|e| format!("net.serve_tls: read cert {cert_path}: {e}"))?;
                let key = std::fs::read(&key_path)
                    .map_err(|e| format!("net.serve_tls: read key {key_path}: {e}"))?;
                serve_http(port, handler_name, program, policy, Some(TlsConfig { cert, key }), ServeOpts::from_env())
            }
            ("net", "serve_ws") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws(port, on_message): port must be Int 0..=65535".into()),
                };
                let handler_name = expect_str(args.get(1))?.to_string();
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws requires a Program reference".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws(port, handler_name, program, policy, registry)
            }
            ("net", "serve_ws_fn") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws_fn(port, subprotocol, handler): port must be Int 0..=65535".into()),
                };
                let subprotocol = expect_str(args.get(1))?.to_string();
                let closure = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn(port, subprotocol, handler): handler must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws_fn requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws_fn(port, subprotocol, closure, program, policy, registry)
            }
            ("net", "serve_ws_fn_auth") => {
                // serve_ws_fn_auth(port, subprotocol, auth, on_message)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws_fn_auth(port, subprotocol, auth, on_message): port must be Int 0..=65535".into()),
                };
                let subprotocol = expect_str(args.get(1))?.to_string();
                let mut it = args.into_iter().skip(2);
                let auth_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_auth(port, subprotocol, auth, on_message): auth must be a closure".into()),
                };
                let handler_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_auth(port, subprotocol, auth, on_message): on_message must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws_fn_auth requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws_fn_auth(
                    port, subprotocol, auth_closure, handler_closure,
                    program, policy, registry,
                )
            }
            ("net", "serve_ws_fn_actor") => {
                // serve_ws_fn_actor(port, subprotocol, name_of, on_message)
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws_fn_actor(port, subprotocol, name_of, on_message): port must be Int 0..=65535".into()),
                };
                let subprotocol = expect_str(args.get(1))?.to_string();
                let mut it = args.into_iter().skip(2);
                let name_of_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_actor(port, subprotocol, name_of, on_message): name_of must be a closure".into()),
                };
                let on_message_closure = match it.next() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("net.serve_ws_fn_actor(port, subprotocol, name_of, on_message): on_message must be a closure".into()),
                };
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws_fn_actor requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws_fn_actor(
                    port, subprotocol, name_of_closure, on_message_closure,
                    program, policy, registry,
                )
            }
            ("net", "dial_ws") => {
                // dial_ws(url, subprotocol, on_open, on_message)
                let url = expect_str(args.first())?.to_string();
                let subprotocol = expect_str(args.get(1))?.to_string();
                let on_open = match args.get(2).cloned() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws(url, subprotocol, on_open, on_message): on_open must be a closure".into(),
                    ),
                };
                let on_message = match args.into_iter().nth(3) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws(url, subprotocol, on_open, on_message): on_message must be a closure".into(),
                    ),
                };
                let program = self.program.clone().ok_or_else(|| {
                    "net.dial_ws requires a Program reference; use DefaultHandler::with_program".to_string()
                })?;
                let policy = self.policy.clone();
                crate::ws::dial_ws(url, subprotocol, on_open, on_message, program, policy)
            }
            ("net", "dial_ws_actor") => {
                // dial_ws_actor(url, subprotocol, name, on_open, on_message)
                let url = expect_str(args.first())?.to_string();
                let subprotocol = expect_str(args.get(1))?.to_string();
                let name = expect_str(args.get(2))?.to_string();
                let on_open = match args.get(3).cloned() {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws_actor(url, subprotocol, name, on_open, on_message): on_open must be a closure".into(),
                    ),
                };
                let on_message = match args.into_iter().nth(4) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err(
                        "net.dial_ws_actor(url, subprotocol, name, on_open, on_message): on_message must be a closure".into(),
                    ),
                };
                let program = self.program.clone().ok_or_else(|| {
                    "net.dial_ws_actor requires a Program reference; use DefaultHandler::with_program".to_string()
                })?;
                let policy = self.policy.clone();
                crate::ws::dial_ws_actor(url, subprotocol, name, on_open, on_message, program, policy)
            }
            ("chat", "broadcast") => {
                let registry = self.chat_registry.as_ref()
                    .ok_or_else(|| "chat.broadcast called outside a net.serve_ws handler".to_string())?;
                let room = expect_str(args.first())?;
                let body = expect_str(args.get(1))?;
                crate::ws::chat_broadcast(registry, room, body);
                Ok(Value::Unit)
            }
            ("chat", "send") => {
                let registry = self.chat_registry.as_ref()
                    .ok_or_else(|| "chat.send called outside a net.serve_ws handler".to_string())?;
                let conn_id = match args.first() {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => return Err("chat.send: conn_id must be non-negative Int".into()),
                };
                let body = expect_str(args.get(1))?;
                Ok(Value::Bool(crate::ws::chat_send(registry, conn_id, body)))
            }
            ("kv", "open") => {
                let path = expect_str(args.first())?.to_string();
                // Honor write-allowlist: opening a Kv writes its
                // backing files at `path`, so the same scoping that
                // applies to `io.write` applies here.
                if !self.policy.allow_fs_write.is_empty() {
                    let p = std::path::Path::new(&path);
                    if !self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
                        return Ok(err(Value::Str(format!(
                            "kv.open: `{path}` outside --allow-fs-write").into())));
                    }
                }
                match sled::open(&path) {
                    Ok(db) => {
                        let handle = next_kv_handle();
                        kv_registry().lock().unwrap().insert(handle, db);
                        Ok(ok(Value::Int(handle as i64)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("kv.open: {e}").into()))),
                }
            }
            ("kv", "close") => {
                let h = expect_kv_handle(args.first())?;
                kv_registry().lock().unwrap().remove(h);
                Ok(Value::Unit)
            }
            ("kv", "get") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.get: closed or unknown Kv handle".to_string())?;
                match db.get(key.as_bytes()) {
                    Ok(Some(ivec)) => Ok(some(Value::Bytes(ivec.to_vec()))),
                    Ok(None) => Ok(none()),
                    Err(e) => Err(format!("kv.get: {e}")),
                }
            }
            ("kv", "put") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_bytes(args.get(2))?.clone();
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.put: closed or unknown Kv handle".to_string())?;
                match db.insert(key.as_bytes(), val) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("kv.put: {e}").into()))),
                }
            }
            ("kv", "delete") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.delete: closed or unknown Kv handle".to_string())?;
                match db.remove(key.as_bytes()) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("kv.delete: {e}").into()))),
                }
            }
            ("kv", "contains") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.contains: closed or unknown Kv handle".to_string())?;
                match db.contains_key(key.as_bytes()) {
                    Ok(present) => Ok(Value::Bool(present)),
                    Err(e) => Err(format!("kv.contains: {e}")),
                }
            }
            ("kv", "list_prefix") => {
                let h = expect_kv_handle(args.first())?;
                let prefix = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.list_prefix: closed or unknown Kv handle".to_string())?;
                let mut keys: Vec<Value> = Vec::new();
                for kv in db.scan_prefix(prefix.as_bytes()) {
                    let (k, _) = kv.map_err(|e| format!("kv.list_prefix: {e}"))?;
                    let s = String::from_utf8_lossy(&k).to_string();
                    keys.push(Value::Str(s.into()));
                }
                Ok(Value::List(keys.into()))
            }
            // ── std.vcs: content-addressed blob store (#5) ──
            // Backed by lex-store's blob CAS (Store::put_blob/get_blob/
            // set_blob_ref/get_blob_ref). Effect `vcs` is gated by the generic
            // ensure_kind_allowed(kind) above. put_blob's sha ==
            // crypto.sha256_str(content), so vcs blobs and loom's SQLite
            // artifacts share ids. We depend on lex-store with the `trace`
            // feature off to avoid a lex-store → lex-trace → lex-runtime cycle.
            ("vcs", "put_blob") => {
                let content = expect_str(args.first())?.to_string();
                match lex_store::Store::open(vcs_store_root())
                    .and_then(|s| s.put_blob(&content)) {
                    Ok(sha) => Ok(ok(Value::Str(sha.into()))),
                    Err(e)  => Ok(err(Value::Str(format!("vcs.put_blob: {e}").into()))),
                }
            }
            ("vcs", "get_blob") => {
                let sha = expect_str(args.first())?.to_string();
                match lex_store::Store::open(vcs_store_root())
                    .and_then(|s| s.get_blob(&sha)) {
                    Ok(content) => Ok(ok(Value::Str(content.into()))),
                    Err(e)      => Ok(err(Value::Str(format!("vcs.get_blob: {e}").into()))),
                }
            }
            ("vcs", "has_blob") => {
                let sha = expect_str(args.first())?.to_string();
                let has = lex_store::Store::open(vcs_store_root())
                    .map(|s| s.has_blob(&sha)).unwrap_or(false);
                Ok(Value::Bool(has))
            }
            ("vcs", "ref_set") => {
                let ns  = expect_str(args.first())?.to_string();
                let key = expect_str(args.get(1))?.to_string();
                let sha = expect_str(args.get(2))?.to_string();
                match lex_store::Store::open(vcs_store_root())
                    .and_then(|s| s.set_blob_ref(&ns, &key, &sha)) {
                    Ok(())  => Ok(ok(Value::Unit)),
                    Err(e)  => Ok(err(Value::Str(format!("vcs.ref_set: {e}").into()))),
                }
            }
            ("vcs", "ref_get") => {
                let ns  = expect_str(args.first())?.to_string();
                let key = expect_str(args.get(1))?.to_string();
                match lex_store::Store::open(vcs_store_root())
                    .and_then(|s| s.get_blob_ref(&ns, &key)) {
                    Ok(sha) => Ok(ok(Value::Str(sha.into()))),
                    Err(e)  => Ok(err(Value::Str(format!("vcs.ref_get: {e}").into()))),
                }
            }
            ("sql", "open") => {
                let path = expect_str(args.first())?.to_string();
                if path.starts_with("postgres://") || path.starts_with("postgresql://") {
                    // Postgres: connect via sync driver; no fs-write policy applies.
                    match postgres::Client::connect(&path, postgres::NoTls) {
                        Ok(client) => {
                            let handle = next_sql_handle();
                            sql_registry().lock().unwrap().insert(handle, SqlConn::Postgres(client));
                            Ok(ok(Value::Int(handle as i64)))
                        }
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.open"))),
                    }
                } else {
                    // SQLite: same shape as `kv.open`; fs-write allowlist applies
                    // (in-memory paths are exempt).
                    if path != ":memory:" && !self.policy.allow_fs_write.is_empty() {
                        let p = std::path::Path::new(&path);
                        if !self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
                            return Ok(err(sql_error(
                                format!("sql.open: `{path}` outside --allow-fs-write"),
                                None, None,
                            )));
                        }
                    }
                    match rusqlite::Connection::open(&path) {
                        Ok(conn) => {
                            let handle = next_sql_handle();
                            sql_registry().lock().unwrap().insert(handle, SqlConn::Sqlite(conn));
                            Ok(ok(Value::Int(handle as i64)))
                        }
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.open"))),
                    }
                }
            }
            ("sql", "close") => {
                let h = expect_sql_handle(args.first())?;
                sql_registry().lock().unwrap().remove(h);
                Ok(Value::Unit)
            }
            ("sql", "exec") => {
                let h = expect_sql_handle(args.first())?;
                let stmt = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.exec: closed or unknown Db handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => {
                        let bound = sqlite_params(&params);
                        let bind: Vec<&dyn rusqlite::ToSql> =
                            bound.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
                        match c.execute(&stmt, rusqlite::params_from_iter(bind.iter())) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.exec"))),
                        }
                    }
                    SqlConn::Postgres(c) => {
                        let pg = pg_param_refs(&params);
                        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                            pg.iter().map(|b| b.as_ref()).collect();
                        match c.execute(pg_rewrite_placeholders(stmt.as_str()).as_str(), &refs) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.exec"))),
                        }
                    }
                }
            }
            ("sql", "query") => {
                let h = expect_sql_handle(args.first())?;
                let stmt_str = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.query: closed or unknown Db handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                Ok(match &mut *conn {
                    SqlConn::Sqlite(c)   => sql_run_query_sqlite(c, &stmt_str, &params),
                    SqlConn::Postgres(c) => sql_run_query_pg(c, &stmt_str, &params),
                })
            }
            // Streaming cursor (#379). Allocates an mpsc-backed cursor
            // handle, spawns a producer thread to ship rows one at a
            // time, and returns `__IterCursor(handle)` wrapped in `Ok`.
            // `iter.next` bytecode dispatches the variant tag and
            // effect-calls `sql.cursor_next` (below) to advance.
            ("sql", "query_iter") => {
                let h = expect_sql_handle(args.first())?;
                let stmt_str = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.query_iter: closed or unknown Db handle".to_string())?;

                // Dispatch producer on the connection kind without
                // holding the SqlRegistry lock — the producer thread
                // owns its own clone of the connection Arc.
                let (sender, receiver) = std::sync::mpsc::sync_channel::<Result<Value, String>>(
                    CURSOR_CHANNEL_CAPACITY,
                );
                let cursor_h = next_cursor_handle();
                cursor_registry().lock().unwrap().insert(cursor_h, receiver);

                let arc_for_thread = Arc::clone(&arc);
                // Decide which producer to spawn based on the
                // connection's variant. We can briefly peek at the
                // variant here without holding the lock for the
                // producer's lifetime — the producer locks again
                // inside its thread function.
                let is_sqlite = matches!(*arc.lock().unwrap(), SqlConn::Sqlite(_));
                std::thread::spawn(move || {
                    if is_sqlite {
                        sqlite_cursor_producer(arc_for_thread, stmt_str, params, sender);
                    } else {
                        pg_cursor_producer(arc_for_thread, stmt_str, params, sender);
                    }
                });

                Ok(ok(Value::Variant {
                    name: "__IterCursor".into(),
                    args: vec![Value::Int(cursor_h as i64)],
                }))
            }
            // Pull one row from the producer; called from
            // `iter.next`'s `__IterCursor` dispatch branch. Returns
            // a Lex `Option[Row]`: `Some(row)` while the producer
            // has more, `None` once the channel closes (producer
            // done, errored, or cursor evicted from the registry).
            ("sql", "cursor_next") => {
                let h = match args.first() {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => return Err("sql.cursor_next: expected cursor handle (Int)".into()),
                };
                let rx_arc = match cursor_registry().lock().unwrap().touch_get(h) {
                    Some(a) => a,
                    None => return Ok(Value::Variant { name: "None".into(), args: vec![] }),
                };
                // Lock the receiver itself (separate from the global
                // registry lock) and block on `recv()`. The producer
                // is on a different thread, so this can sleep without
                // contention beyond the per-cursor mutex.
                let recv_result = {
                    let rx = match rx_arc.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    rx.recv()
                };
                match recv_result {
                    Ok(Ok(row)) => Ok(Value::Variant {
                        name: "Some".into(),
                        args: vec![row],
                    }),
                    Ok(Err(_)) | Err(_) => {
                        // Channel closed (producer done) or row error
                        // — drop the registry entry and signal None
                        // so callers stop polling.
                        cursor_registry().lock().unwrap().remove(h);
                        Ok(Value::Variant { name: "None".into(), args: vec![] })
                    }
                }
            }
            // Transactions: begin issues BEGIN SQL on the connection;
            // commit/rollback issue COMMIT/ROLLBACK. SqlTx reuses the
            // same Int handle as Db — the type system enforces correct
            // usage; the runtime treats both as the same registry key.
            ("sql", "begin") => {
                let h = expect_sql_handle(args.first())?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.begin: closed or unknown Db handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => match c.execute_batch("BEGIN") {
                        Ok(()) => Ok(ok(Value::Int(h as i64))),
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.begin"))),
                    },
                    SqlConn::Postgres(c) => match c.batch_execute("BEGIN") {
                        Ok(()) => Ok(ok(Value::Int(h as i64))),
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.begin"))),
                    },
                }
            }
            ("sql", "commit") => {
                let h = expect_sql_handle(args.first())?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.commit: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => match c.execute_batch("COMMIT") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.commit"))),
                    },
                    SqlConn::Postgres(c) => match c.batch_execute("COMMIT") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.commit"))),
                    },
                }
            }
            ("sql", "rollback") => {
                let h = expect_sql_handle(args.first())?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.rollback: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => match c.execute_batch("ROLLBACK") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.rollback"))),
                    },
                    SqlConn::Postgres(c) => match c.batch_execute("ROLLBACK") {
                        Ok(()) => Ok(ok(Value::Unit)),
                        Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.rollback"))),
                    },
                }
            }
            ("sql", "exec_tx") => {
                let h = expect_sql_handle(args.first())?;
                let stmt = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.exec_tx: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                match &mut *conn {
                    SqlConn::Sqlite(c) => {
                        let bound = sqlite_params(&params);
                        let bind: Vec<&dyn rusqlite::ToSql> =
                            bound.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
                        match c.execute(&stmt, rusqlite::params_from_iter(bind.iter())) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(sqlite_err_to_sql_error(e, "sql.exec_tx"))),
                        }
                    }
                    SqlConn::Postgres(c) => {
                        let pg = pg_param_refs(&params);
                        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                            pg.iter().map(|b| b.as_ref()).collect();
                        match c.execute(pg_rewrite_placeholders(stmt.as_str()).as_str(), &refs) {
                            Ok(n)  => Ok(ok(Value::Int(n as i64))),
                            Err(e) => Ok(err(pg_err_to_sql_error(e, "sql.exec_tx"))),
                        }
                    }
                }
            }
            ("sql", "query_tx") => {
                let h = expect_sql_handle(args.first())?;
                let stmt_str = expect_str(args.get(1))?.to_string();
                let params = expect_sql_params(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.query_tx: closed or unknown SqlTx handle".to_string())?;
                let mut conn = arc.lock().unwrap();
                Ok(match &mut *conn {
                    SqlConn::Sqlite(c)   => sql_run_query_sqlite(c, &stmt_str, &params),
                    SqlConn::Postgres(c) => sql_run_query_pg(c, &stmt_str, &params),
                })
            }
            ("sql", "get_str") => Ok(sql_get_col(&args, |v| match v {
                Value::Str(s) => Some(Value::Str(s.clone())),
                Value::Int(n) => Some(Value::Str(n.to_string().into())),
                _ => None,
            })?),
            ("sql", "get_int") => Ok(sql_get_col(&args, |v| match v {
                Value::Int(n) => Some(Value::Int(*n)),
                Value::Float(f) => Some(Value::Int(*f as i64)),
                _ => None,
            })?),
            ("sql", "get_float") => Ok(sql_get_col(&args, |v| match v {
                Value::Float(f) => Some(Value::Float(*f)),
                Value::Int(n)   => Some(Value::Float(*n as f64)),
                _ => None,
            })?),
            ("sql", "get_bool") => Ok(sql_get_col(&args, |v| match v {
                Value::Bool(b)  => Some(Value::Bool(*b)),
                Value::Int(n)   => Some(Value::Bool(*n != 0)),
                _ => None,
            })?),

            // ── std.redis (#533) ─────────────────────────────────────────
            //
            // ConnRedis is an opaque Int handle into the global RedisRegistry.
            // All ops carry [net] — Redis is a TCP service.
            //
            // subscribe/psubscribe open a *dedicated* connection so they don't
            // interfere with the handle's regular connection. Redis disallows
            // non-Pub/Sub commands on a subscribed connection.
            ("redis", "connect") => {
                let url = expect_str(args.first())?.to_string();
                self.ensure_host_allowed(&url)?;
                match redis::Client::open(url.as_str()) {
                    Ok(client) => match client.get_connection() {
                        Ok(conn) => {
                            let handle = next_redis_handle();
                            redis_registry().lock().unwrap().insert(handle, RedisEntry { url, conn });
                            Ok(ok(Value::Int(handle as i64)))
                        }
                        Err(e) => Ok(err(Value::Str(format!("redis.connect: {e}").into()))),
                    },
                    Err(e) => Ok(err(Value::Str(format!("redis.connect: {e}").into()))),
                }
            }
            ("redis", "close") => {
                let h = expect_redis_handle(args.first())?;
                redis_registry().lock().unwrap().remove(h);
                Ok(Value::Unit)
            }
            ("redis", "get") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.get: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                match entry.conn.get::<_, Option<String>>(&key) {
                    Ok(Some(v)) => Ok(some(Value::Str(v.into()))),
                    Ok(None)    => Ok(none()),
                    Err(e)      => Err(format!("redis.get: {e}")),
                }
            }
            ("redis", "set") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.set: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.set::<_, _, ()>(&key, &val)
                    .map_err(|e| format!("redis.set: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "set_ex") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let ttl = expect_int(args.get(3))?;
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.set_ex: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.set_ex::<_, _, ()>(&key, &val, ttl as u64)
                    .map_err(|e| format!("redis.set_ex: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "del") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.del: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.del::<_, ()>(&key)
                    .map_err(|e| format!("redis.del: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "exists") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.exists: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let present: bool = entry.conn.exists(&key)
                    .map_err(|e| format!("redis.exists: {e}"))?;
                Ok(Value::Bool(present))
            }
            ("redis", "expire") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let ttl = expect_int(args.get(2))?;
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.expire: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.expire::<_, ()>(&key, ttl)
                    .map_err(|e| format!("redis.expire: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "publish") => {
                let h = expect_redis_handle(args.first())?;
                let channel = expect_str(args.get(1))?.to_string();
                let msg = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.publish: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.publish(&channel, &msg)
                    .map_err(|e| format!("redis.publish: {e}"))?;
                Ok(Value::Int(n))
            }
            // subscribe / psubscribe: blocking loops on dedicated connections.
            // Each inbound message calls the Lex closure in a fresh VM built
            // from `self.program` — same pattern as net.serve_fn's per-request
            // dispatch. Returns Unit (Nil) only if the connection drops.
            ("redis", "subscribe") => {
                let h = expect_redis_handle(args.first())?;
                let channel = expect_str(args.get(1))?.to_string();
                let closure = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("redis.subscribe: handler must be a Closure".into()),
                };
                let program = self.program.clone()
                    .ok_or("redis.subscribe: no program; call DefaultHandler::with_program")?;
                let policy = self.policy.clone();
                let url = redis_registry().lock().unwrap()
                    .get_url(h)
                    .ok_or("redis.subscribe: closed or unknown ConnRedis handle")?;
                let client = redis::Client::open(url.as_str())
                    .map_err(|e| format!("redis.subscribe: {e}"))?;
                let mut conn = client.get_connection()
                    .map_err(|e| format!("redis.subscribe: {e}"))?;
                let mut pubsub = conn.as_pubsub();
                pubsub.subscribe(&channel)
                    .map_err(|e| format!("redis.subscribe: {e}"))?;
                loop {
                    let msg = pubsub.get_message()
                        .map_err(|e| format!("redis.subscribe: {e}"))?;
                    let ch: String = msg.get_channel_name().to_string();
                    let payload: String = msg.get_payload()
                        .map_err(|e| format!("redis.subscribe: payload: {e}"))?;
                    let handler = DefaultHandler::new(policy.clone())
                        .with_program(Arc::clone(&program));
                    let mut vm = Vm::with_handler(&program, Box::new(handler));
                    vm.invoke_closure_value(closure.clone(), vec![
                        Value::Str(ch.into()),
                        Value::Str(payload.into()),
                    ]).map_err(|e| format!("redis.subscribe: handler: {e:?}"))?;
                }
            }
            ("redis", "psubscribe") => {
                let h = expect_redis_handle(args.first())?;
                let pattern = expect_str(args.get(1))?.to_string();
                let closure = match args.into_iter().nth(2) {
                    Some(c @ Value::Closure { .. }) => c,
                    _ => return Err("redis.psubscribe: handler must be a Closure".into()),
                };
                let program = self.program.clone()
                    .ok_or("redis.psubscribe: no program; call DefaultHandler::with_program")?;
                let policy = self.policy.clone();
                let url = redis_registry().lock().unwrap()
                    .get_url(h)
                    .ok_or("redis.psubscribe: closed or unknown ConnRedis handle")?;
                let client = redis::Client::open(url.as_str())
                    .map_err(|e| format!("redis.psubscribe: {e}"))?;
                let mut conn = client.get_connection()
                    .map_err(|e| format!("redis.psubscribe: {e}"))?;
                let mut pubsub = conn.as_pubsub();
                pubsub.psubscribe(&pattern)
                    .map_err(|e| format!("redis.psubscribe: {e}"))?;
                loop {
                    let msg = pubsub.get_message()
                        .map_err(|e| format!("redis.psubscribe: {e}"))?;
                    let pat: String = msg.get_pattern()
                        .ok()
                        .and_then(|v: Option<String>| v)
                        .unwrap_or_else(|| pattern.clone());
                    let ch: String = msg.get_channel_name().to_string();
                    let payload: String = msg.get_payload()
                        .map_err(|e| format!("redis.psubscribe: payload: {e}"))?;
                    let handler = DefaultHandler::new(policy.clone())
                        .with_program(Arc::clone(&program));
                    let mut vm = Vm::with_handler(&program, Box::new(handler));
                    vm.invoke_closure_value(closure.clone(), vec![
                        Value::Str(pat.into()),
                        Value::Str(ch.into()),
                        Value::Str(payload.into()),
                    ]).map_err(|e| format!("redis.psubscribe: handler: {e:?}"))?;
                }
            }
            ("redis", "lpush") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.lpush: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.lpush(&key, &val)
                    .map_err(|e| format!("redis.lpush: {e}"))?;
                Ok(Value::Int(n))
            }
            ("redis", "rpush") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.rpush: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.rpush(&key, &val)
                    .map_err(|e| format!("redis.rpush: {e}"))?;
                Ok(Value::Int(n))
            }
            ("redis", "brpop") => {
                // timeout=0 means block indefinitely; the Lex runtime does not
                // treat this as a hung effect — it is the caller's intent.
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let timeout = expect_int(args.get(2))?;
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.brpop: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                // brpop returns Option<(String, String)>: (key, value).
                // We surface only the value to the Lex caller.
                let result: Option<(String, String)> = entry.conn
                    .brpop(&key, timeout as f64)
                    .map_err(|e| format!("redis.brpop: {e}"))?;
                match result {
                    Some((_, v)) => Ok(some(Value::Str(v.into()))),
                    None         => Ok(none()),
                }
            }
            ("redis", "llen") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.llen: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let n: i64 = entry.conn.llen(&key)
                    .map_err(|e| format!("redis.llen: {e}"))?;
                Ok(Value::Int(n))
            }
            ("redis", "hset") => {
                let h = expect_redis_handle(args.first())?;
                let key   = expect_str(args.get(1))?.to_string();
                let field = expect_str(args.get(2))?.to_string();
                let val   = expect_str(args.get(3))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hset: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.hset::<_, _, _, ()>(&key, &field, &val)
                    .map_err(|e| format!("redis.hset: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "hget") => {
                let h = expect_redis_handle(args.first())?;
                let key   = expect_str(args.get(1))?.to_string();
                let field = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hget: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                match entry.conn.hget::<_, _, Option<String>>(&key, &field) {
                    Ok(Some(v)) => Ok(some(Value::Str(v.into()))),
                    Ok(None)    => Ok(none()),
                    Err(e)      => Err(format!("redis.hget: {e}")),
                }
            }
            ("redis", "hdel") => {
                let h = expect_redis_handle(args.first())?;
                let key   = expect_str(args.get(1))?.to_string();
                let field = expect_str(args.get(2))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hdel: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                entry.conn.hdel::<_, _, ()>(&key, &field)
                    .map_err(|e| format!("redis.hdel: {e}"))?;
                Ok(Value::Unit)
            }
            ("redis", "hgetall") => {
                let h = expect_redis_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let mut reg = redis_registry().lock().unwrap();
                let entry = reg.touch_get_mut(h)
                    .ok_or_else(|| "redis.hgetall: closed or unknown ConnRedis handle".to_string())?;
                use redis::Commands;
                let map: std::collections::HashMap<String, String> = entry.conn
                    .hgetall(&key)
                    .map_err(|e| format!("redis.hgetall: {e}"))?;
                let pairs: Vec<Value> = map.into_iter()
                    .map(|(k, v)| Value::Tuple(vec![Value::Str(k.into()), Value::Str(v.into())]))
                    .collect();
                Ok(Value::List(pairs.into()))
            }

            // `proc.spawn` was removed with the `std.proc` module (#678);
            // the blocking-capture path now lives at `process.run`, handled
            // in the `kind == "process"` block above.
            other => Err(format!("unsupported effect {}.{}", other.0, other.1)),
        }
    }

    /// `list.par_map` worker-handler factory (#305 slice 2).
    ///
    /// Builds a fresh `DefaultHandler` per worker that shares the
    /// budget pool with the parent (`Arc<AtomicU64>`) so a parallel
    /// batch can't escape the run-wide budget ceiling. Other state
    /// is intentionally split per-worker:
    ///
    /// - `sink`: a `StdoutSink` per worker. Tests that capture
    ///   output via a `SharedSink` wrapped in `Arc<Mutex<…>>` see
    ///   each worker as a fresh handler. Print interleaving on
    ///   stdout is acceptable; tests that need ordered capture run
    ///   workloads serially anyway.
    /// - `mcp_clients`: a fresh per-worker LRU cache. The parent's
    ///   subprocess handles can't be shared across threads without
    ///   mutex-serialising every MCP call, which would defeat the
    ///   parallelism. Cache hit rate is sub-optimal across the
    ///   first call per worker; warmed caches still amortise within
    ///   a worker.
    /// - `chat_registry`: cloned `Arc<ChatRegistry>` so all workers
    ///   route into the same chat dispatch layer.
    /// - `program`: cloned `Arc<Program>` so `net.serve` (if a
    ///   worker invokes it) sees the same compiled program.
    fn spawn_for_worker(&self) -> Option<Box<dyn lex_bytecode::vm::EffectHandler + Send>> {
        let mut fresh = DefaultHandler::new(self.policy.clone());
        // Share the budget pool atomically — slice 2's correctness
        // contract: parallel work counts against the same ceiling.
        fresh.budget_remaining = std::sync::Arc::clone(&self.budget_remaining);
        fresh.budget_ceiling = self.budget_ceiling;
        fresh.read_root = self.read_root.clone();
        fresh.program = self.program.clone();
        fresh.chat_registry = self.chat_registry.clone();
        // #305 slice 3: share the stream registry across workers so
        // a stream produced on one thread (or the parent) is
        // consumable on any other. The registry is already
        // `Arc<Mutex<…>>` so concurrent access is safe.
        fresh.streams = std::sync::Arc::clone(&self.streams);
        fresh.next_stream_id = std::sync::Arc::clone(&self.next_stream_id);
        fresh.program_args = self.program_args.clone();
        Some(Box::new(fresh))
    }
}
