//! `lex run` / `lex trace` / `lex diff`: execute a program under the runtime policy, record traces, and diff two runs.

use super::*;

pub(super) fn cmd_run(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let f = parse_run_flags(args)?;
    // #227 follow-up: when `--from-store` is set, the first
    // positional is the function name (no path needed). Otherwise
    // the legacy shape `lex run <file> <fn> [args]` applies.
    let (source_label, func, arg_positional_start) = if f.from_store.is_some() {
        let func = f.positional.first().ok_or_else(|| anyhow!(
            "usage: lex run --from-store STAGE_ID [--require-signed] [--trusted-key HEX] <fn> [args]"))?;
        (
            format!("store:{}", f.from_store.as_deref().unwrap()),
            func.clone(),
            1,
        )
    } else {
        let path = f.positional.first().ok_or_else(|| anyhow!(
            "usage: lex run [policy] [--from-canonical] <file> [fn] [args] or <file> -- [program args]"))?;
        // When `--` was the separator, positional has only the file path;
        // default to calling `main` with empty vargs and program_args in io.argv().
        let func = f
            .positional
            .get(1)
            .cloned()
            .unwrap_or_else(|| "main".to_string());
        (path.clone(), func, 2)
    };
    let policy = &f.policy;
    if f.dry_run {
        let actions = vec![serde_json::json!({
            "action": "execute",
            "source": &source_label,
            "function": func,
            "args": &f.positional[arg_positional_start..],
            "policy": {
                "allow_effects": policy.allow_effects.iter().collect::<Vec<_>>(),
                "allow_fs_read": policy.allow_fs_read.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "allow_fs_write": policy.allow_fs_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "allow_net_host": &policy.allow_net_host,
                "allow_approval": &policy.allow_approval,
                "budget": policy.budget,
            },
            "trace": f.trace,
            "max_steps": f.max_steps,
        })];
        acli::emit_dry_run(
            "run",
            fmt,
            &format!("would call `{func}` in {source_label}"),
            actions,
        );
    }
    // #206 slice 3 (text/canonical paths) or #227 follow-up
    // (store path). Each produces the same Vec<Stage>; the typecheck
    // and compile pipeline is identical from this point on.
    let mut stages = if let Some(stage_id) = &f.from_store {
        load_stages_from_store(stage_id, f.require_signed, f.trusted_key.as_deref())?
    } else {
        load_stages(&source_label, f.from_canonical)?
    };
    // #168: rewrite stdlib parse calls during type-check so the
    // runtime sees the strict (validated) shape.
    if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
        let arr: Vec<serde_json::Value> = errs
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let data = serde_json::json!({ "phase": "type-check", "errors": arr });
        acli::emit_or_text("run", data, fmt, || {
            for e in &errs {
                if let Ok(j) = serde_json::to_string(e) {
                    eprintln!("{j}");
                }
            }
        });
        std::process::exit(2);
    }
    let bc = compile_program(&stages);

    if let Err(violations) = check_policy(&bc, policy) {
        let arr: Vec<serde_json::Value> = violations
            .iter()
            .map(|v| serde_json::to_value(v).unwrap())
            .collect();
        let data = serde_json::json!({ "phase": "policy", "violations": arr });
        acli::emit_or_text("run", data, fmt, || {
            for v in &violations {
                if let Ok(j) = serde_json::to_string(v) {
                    eprintln!("{j}");
                }
            }
        });
        std::process::exit(3);
    }

    let bc = std::sync::Arc::new(bc);
    let handler = DefaultHandler::new(f.policy.clone())
        .with_program(std::sync::Arc::clone(&bc))
        .with_program_args(f.program_args.clone());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    if let Some(n) = f.max_steps {
        // `--max-steps 0` = unbounded (no opcode cap). The step counter is a
        // DoS guard for *untrusted* code (the agent-tool sandbox); trusted runs
        // shouldn't have to guess an opcode budget they can't estimate, so 0
        // opts out explicitly. u64::MAX is effectively unbounded for any real run.
        vm.set_step_limit(if n == 0 { u64::MAX } else { n });
    }
    // #465: install the JIT tier as a hook on the Vm. Eligible
    // functions (pure-int arith subset) compile to native code on
    // first call; everything else flows through the interpreter
    // unchanged. JITed code now accounts loop iterations against
    // the same `Vm::steps` counter the interpreter uses (the
    // architectural fix that closed the bypass cursor[bot] flagged
    // on #608/#609/#707), so `--jit` and `--max-steps` compose
    // cleanly and the default 10M cap is honored on both paths.
    // A construction failure (e.g. the `cranelift` feature was
    // off) propagates the JitError as a user-visible run error
    // rather than silently falling back, so a `--jit` invocation
    // that can't actually JIT is loud.
    if f.jit {
        let tier =
            lex_jit::JitTier::new(&bc).map_err(|e| anyhow!("--jit: constructing JIT tier: {e}"))?;
        vm.set_jit_hook(Some(Box::new(tier)));
    }
    let recorder = lex_trace::Recorder::new();
    let trace_handle = recorder.handle();
    if f.trace {
        vm.set_tracer(Box::new(recorder));
    }
    // #257: snapshot the default branch's head before the run so we
    // can attribute any ops committed during the run back to the
    // run's `run_id` via Trace attestations. `pre_run_head` is
    // `None` for a fresh store; that's fine — `record_run_committed_ops_since`
    // treats `None` as "every reachable op is post-run", which is
    // the right behavior on an empty pre-run history.
    let pre_run_head = if f.trace {
        let store = lex_store::Store::open(default_store_root())?;
        store
            .get_branch(lex_store::DEFAULT_BRANCH)
            .map_err(|e| anyhow!("reading branch: {e}"))?
            .and_then(|b| b.head_op)
    } else {
        None
    };

    // When `--` separator was used, program_args holds the argv and vargs is empty.
    let vargs: Vec<Value> = if !f.program_args.is_empty() {
        Vec::new()
    } else {
        f.positional[arg_positional_start..]
            .iter()
            .map(|a| {
                let v: serde_json::Value =
                    serde_json::from_str(a).with_context(|| format!("arg `{a}` must be JSON"))?;
                Ok(json_to_value(&v))
            })
            .collect::<Result<Vec<_>>>()?
    };
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let result = vm.call(&func, vargs);
    let ended = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut trace_id: Option<String> = None;
    if f.trace {
        let store = lex_store::Store::open(default_store_root())?;
        let (root_out, root_err) = match &result {
            Ok(v) => (Some(value_to_json(v)), None),
            Err(e) => (None, Some(format!("{e}"))),
        };
        let tree = trace_handle.finalize(
            func.clone(),
            serde_json::Value::Null,
            root_out,
            root_err,
            started,
            ended,
        );
        let id = store.save_trace(&tree)?;
        // #246: emit a `Trace` attestation linking the run to the
        // entry stage. Skipped silently if the entry function isn't
        // resolvable to a stage in the loaded program — `lex run`
        // accepts plain `.lex` files that may carry sigs not yet
        // published to the store, and the audit hook is informational
        // rather than load-bearing.
        if let Some(entry_stage_id) = entry_stage_id_for(&stages, &func) {
            let attestation = lex_vcs::Attestation::new(
                entry_stage_id,
                None,
                None,
                lex_vcs::AttestationKind::Trace {
                    run_id: id.clone(),
                    root_target: func.clone(),
                },
                match &result {
                    Ok(_) => lex_vcs::AttestationResult::Passed,
                    Err(e) => lex_vcs::AttestationResult::Failed {
                        detail: format!("{e}"),
                    },
                },
                trace_producer(),
                None,
            );
            // Use the store's attestation log helper so the file
            // layout is consistent with `lex publish`'s emissions.
            store
                .attestation_log()
                .map_err(|e| anyhow!("opening attestation log: {e}"))?
                .put(&attestation)
                .map_err(|e| anyhow!("recording trace attestation: {e}"))?;
        }
        // #257: emit `Trace` attestations with `op_id` set for any
        // op committed during the run. Walks `ops_since(post_head,
        // pre_run_head)` on the default branch — the only branch
        // `lex run` interacts with today. Empty for the common case
        // where the program doesn't commit ops.
        let att_result = match &result {
            Ok(_) => lex_vcs::AttestationResult::Passed,
            Err(e) => lex_vcs::AttestationResult::Failed {
                detail: format!("{e}"),
            },
        };
        let n_op_traces = store
            .record_run_committed_ops_since(
                &id,
                &func,
                lex_store::DEFAULT_BRANCH,
                pre_run_head.as_ref(),
                att_result,
                trace_producer(),
            )
            .map_err(|e| anyhow!("recording op traces: {e}"))?;
        if n_op_traces > 0 && !matches!(fmt, OutputFormat::Json) {
            eprintln!("trace attestations: {n_op_traces} op(s) linked to run");
        }
        trace_id = Some(id.clone());
        if !matches!(fmt, OutputFormat::Json) {
            eprintln!("trace saved: {id}");
        }
    }
    let r = result.map_err(|e| anyhow!("runtime: {e}"))?;
    let result_json = value_to_json(&r);
    let data = match &trace_id {
        Some(id) => serde_json::json!({ "result": result_json, "trace_id": id }),
        None => serde_json::json!({ "result": result_json }),
    };
    acli::emit_or_text("run", data, fmt, || {
        println!("{}", value_to_json_string(&r))
    });
    Ok(())
}

/// Parsed arguments for `lex run`.
#[derive(Default)]
pub(super) struct RunFlags {
    pub(super) policy: Policy,
    pub(super) positional: Vec<String>,
    pub(super) trace: bool,
    pub(super) max_steps: Option<u64>,
    pub(super) dry_run: bool,
    pub(super) from_canonical: bool,
    /// `--from-store STAGE_ID` (#227 follow-up). Loads the stage's
    /// canonical AST out of the store instead of reading a file. The
    /// fn-arg must name a function that exists in the loaded stage.
    pub(super) from_store: Option<String>,
    /// Refuse to run an unsigned stage (only meaningful with
    /// `--from-store`). Implied by `--trusted-key`.
    pub(super) require_signed: bool,
    /// Hex Ed25519 public key the stage must be signed by.
    pub(super) trusted_key: Option<String>,
    /// Args after `--` separator, passed to `io.argv()` in the program.
    pub(super) program_args: Vec<String>,
    /// #465: route eligible functions through the Cranelift JIT
    /// tier (`lex_jit::JitTier`). Default-off because the JIT only
    /// pays off on numeric hot paths; programs without those run
    /// the same speed either way and pay a small per-call wrapper
    /// cost. Enabled with `--jit`.
    pub(super) jit: bool,
}

pub(super) fn parse_run_flags(args: &[String]) -> Result<RunFlags> {
    let mut f = RunFlags {
        policy: Policy::pure(),
        ..Default::default()
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--allow-effects" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-effects needs a value"))?;
                f.policy.allow_effects = val
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect::<BTreeSet<_>>();
                i += 2;
            }
            "--allow-fs-read" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-fs-read needs a value"))?;
                f.policy.allow_fs_read.push(PathBuf::from(val));
                i += 2;
            }
            "--allow-fs-write" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-fs-write needs a value"))?;
                f.policy.allow_fs_write.push(PathBuf::from(val));
                i += 2;
            }
            "--allow-net-host" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-net-host needs a value"))?;
                f.policy.allow_net_host.push(val.clone());
                i += 2;
            }
            "--allow-proc" => {
                // Comma-separated binary basenames the [proc] effect
                // is allowed to spawn. Read SECURITY.md before granting.
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-proc needs a value"))?;
                for name in val.split(',').filter(|s| !s.is_empty()) {
                    f.policy.allow_proc.push(name.to_string());
                }
                i += 2;
            }
            "--allow-approval" => {
                // Comma-separated scopes the [approval] effect may
                // request (e.g. "payment,deploy"). Empty --allow-effects
                // approval + no --allow-approval = any scope (wildcard).
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-approval needs a value"))?;
                for scope in val.split(',').filter(|s| !s.is_empty()) {
                    f.policy.allow_approval.push(scope.to_string());
                }
                i += 2;
            }
            "--budget" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--budget needs a value"))?;
                f.policy.budget = Some(val.parse().context("--budget must be an integer")?);
                i += 2;
            }
            "--max-steps" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--max-steps needs a value"))?;
                f.max_steps = Some(val.parse().context("--max-steps must be an integer")?);
                i += 2;
            }
            "--trace" => {
                f.trace = true;
                i += 1;
            }
            "--dry-run" => {
                f.dry_run = true;
                i += 1;
            }
            "--jit" => {
                f.jit = true;
                i += 1;
            }
            "--from-canonical" => {
                // #206 slice 3: read the program as canonical-AST
                // bytes instead of `.lex` text. The path argument
                // points to the bytes file (or `-` for stdin); the
                // text parser is bypassed entirely on this path.
                f.from_canonical = true;
                i += 1;
            }
            "--from-store" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--from-store needs a stage_id"))?;
                f.from_store = Some(val.clone());
                i += 2;
            }
            "--require-signed" => {
                f.require_signed = true;
                i += 1;
            }
            "--trusted-key" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--trusted-key needs a hex value"))?;
                f.trusted_key = Some(val.clone());
                f.require_signed = true;
                i += 2;
            }
            "--" => {
                // Everything after `--` is passed to io.argv() in the program.
                // `lex run <file> -- [program args]` calls `main()`.
                f.program_args = args[i + 1..].to_vec();
                break;
            }
            _ => {
                f.positional.push(a.clone());
                i += 1;
            }
        }
    }
    Ok(f)
}

/// `lex trace <run_id>` — load the trace tree by run id (existing).
/// `lex trace --op <op_id>` (#246) — list every `AttestationKind::Trace`
/// attestation whose `op_id` field matches. Populated by the
/// ops-during-run pipeline (#257): when `lex run --trace` finds
/// any op committed during the run, it emits per-stage Trace
/// attestations with `op_id: Some(...)` set, which this filter
/// surfaces. The entry-point Trace attestation (no op_id) is not
/// returned — it's not associated with a single committed op.
pub(super) fn cmd_trace(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // --op flag form first.
    let mut op_filter: Option<String> = None;
    let mut store_root: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--op" => {
                op_filter = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--op needs an op_id"))?
                        .clone(),
                );
                i += 2;
            }
            "--store" => {
                store_root = Some(PathBuf::from(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--store needs a path"))?,
                ));
                i += 2;
            }
            other if !other.starts_with("--") => {
                positional.push(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let root = store_root.unwrap_or_else(default_store_root);
    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;

    if let Some(op_id) = op_filter {
        let log = store.attestation_log()?;
        let traces: Vec<lex_vcs::Attestation> = log
            .list_all()?
            .into_iter()
            .filter(|a| {
                matches!(a.kind, lex_vcs::AttestationKind::Trace { .. })
                    && a.op_id.as_deref() == Some(op_id.as_str())
            })
            .collect();
        let data = serde_json::json!({
            "op_id": op_id,
            "count": traces.len(),
            "traces": serde_json::to_value(&traces)?,
        });
        let listing = traces.clone();
        acli::emit_or_text("trace", data, fmt, move || {
            if listing.is_empty() {
                println!("(no Trace attestations for op {op_id})");
                return;
            }
            for a in &listing {
                if let lex_vcs::AttestationKind::Trace {
                    run_id,
                    root_target,
                } = &a.kind
                {
                    println!("{run_id}\t{root_target}\tat={}", a.timestamp);
                }
            }
        });
        return Ok(());
    }

    // Positional path: load and dump the trace tree.
    let id = positional.first().ok_or_else(|| {
        anyhow!("usage: lex trace <run_id> | lex trace --op <op_id> [--store DIR]")
    })?;
    let tree = store.load_trace(id)?;
    let data = serde_json::to_value(&tree)?;
    acli::emit_or_text("trace", data.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
    });
    Ok(())
}

/// Find the `stage_id` of the entry-point function in a parsed
/// program. Used by `lex run --trace` (#246) to attach a stage
/// reference to the emitted [`AttestationKind::Trace`]. Returns
/// `None` when the function name doesn't match any FnDecl in the
/// program — typically because the caller passed a stdlib name or
/// a function the file doesn't actually define.
pub(super) fn entry_stage_id_for(stages: &[lex_ast::Stage], func: &str) -> Option<String> {
    for stage in stages {
        if let lex_ast::Stage::FnDecl(fd) = stage {
            if fd.name == func {
                return lex_ast::stage_id(stage);
            }
        }
    }
    None
}

/// Producer for the `Trace` attestation emitted by `lex run --trace`
/// (#246). Tagged as `lex-cli` (not `lex-store`) because the run
/// command — not the store gate — is what notices a tracer was
/// active.
pub(super) fn trace_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex run --trace".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

pub(super) fn cmd_diff(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let a = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex diff <run_a> <run_b>"))?;
    let b = args
        .get(1)
        .ok_or_else(|| anyhow!("missing second run id"))?;
    let store = lex_store::Store::open(default_store_root())?;
    let ta = store.load_trace(a)?;
    let tb = store.load_trace(b)?;
    let data = match lex_trace::diff_runs(&ta, &tb) {
        Some(d) => serde_json::to_value(&d)?,
        None => serde_json::json!({ "divergence": null }),
    };
    acli::emit_or_text("diff", data.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
    });
    Ok(())
}
