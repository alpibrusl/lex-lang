//! `lex agent-tool`: generate, check and run a single-purpose tool from a request or body under an effect allow-list.

use super::*;

pub(super) struct AgentToolOpts {
    pub(super) allowed_effects: Vec<String>,
    pub(super) user_input: String,
    pub(super) body_source: BodySource,
    pub(super) api_key: Option<String>,
    pub(super) model: String,
    pub(super) show_source: bool,
    /// Cap on opcode dispatches before the VM aborts with
    /// `step limit exceeded`. Defends against agent-emitted DoS
    /// (`list.fold(list.range(0, 1e9), …)`). Default 1_000_000 —
    /// generous enough for analytics + linreg, tight enough that
    /// runaway loops surface in <1s.
    pub(super) max_steps: u64,
    /// Per-path scope on `[fs_read]` / `[io]` reads. Empty = any.
    pub(super) allow_fs_read: Vec<PathBuf>,
    /// Per-host scope on `[net]`. Empty = any host. Useful when a
    /// tool needs to call api.openai.com but should not be able to
    /// POST to attacker.example.com.
    pub(super) allow_net_host: Vec<String>,
    /// Path to a JSON file of `[{"input": "...", "expected": "..."}, ...]`
    /// pairs. When set, the tool runs once per case and is rejected
    /// if any output mismatches `expected`. Closes the well-typed-but-
    /// wrong-behavior gap: the type system says what code touches; the
    /// examples say what it should return.
    pub(super) examples_file: Option<PathBuf>,
    /// Path to a Spec file (`spec name { forall …: <bool expr> }`) to
    /// prove against the emitted body before trusting it. Counterexamples
    /// abort with exit 5 (same family as examples-failed); inconclusive
    /// proofs abort with exit 6 unless `--spec-allow-inconclusive` is
    /// set. This is the strongest behavioral check available — it lifts
    /// rung 2 from "show me the answer for these N cases" to "show me
    /// the answer for *all* cases the spec ranges over."
    pub(super) spec_file: Option<PathBuf>,
    /// If true, an inconclusive Spec proof doesn't abort the run.
    /// Useful when SMT can't decide a property but you still want
    /// to ship; the spec's own evidence record stays in the trace.
    pub(super) spec_allow_inconclusive: bool,
    /// Trials for randomized fallback when SMT can't decide.
    pub(super) spec_trials: u32,
    /// Optional second body to compare against. When set, both bodies
    /// run on each input (single `--input` or every entry from
    /// `--examples`); any output divergence aborts with exit 7.
    /// Catches model-version regressions when v1's emission and v2's
    /// emission disagree on at least one case the host cares about.
    pub(super) diff_body_source: Option<BodySource>,
    /// Store root for attestation persistence (#132). When set,
    /// every verification step (`--examples`, `--spec`, `--diff-body`,
    /// and the final sandboxed run) emits an attestation against
    /// the StageId of the agent-emitted `tool` fn. None ⇒ verifications
    /// run as before with no persistence.
    pub(super) store_root: Option<PathBuf>,
}

pub(super) enum BodySource {
    Request(String),
    Literal(String),
    File(PathBuf),
}

pub(super) fn cmd_agent_tool(args: &[String]) -> Result<()> {
    let opts = parse_agent_tool_args(args)?;

    // 1) Get the tool body — from Claude or supplied verbatim.
    let body = match &opts.body_source {
        BodySource::Literal(b) => b.clone(),
        BodySource::File(p) => {
            fs::read_to_string(p).with_context(|| format!("read body from {}", p.display()))?
        }
        BodySource::Request(req) => {
            let api_key = opts
                .api_key
                .clone()
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .ok_or_else(|| {
                    anyhow!(
                        "--request needs ANTHROPIC_API_KEY (or pass --api-key); \
                     for offline use try --body or --body-file"
                    )
                })?;
            call_claude_for_body(req, &opts.allowed_effects, &api_key, &opts.model)?
        }
    };
    let body = strip_code_fences(&body);

    if opts.show_source {
        eprintln!("→ tool body:");
        for l in body.lines() {
            eprintln!("    {l}");
        }
    }

    // 2) Splice into the template.
    let src = build_tool_program(&body, &opts.allowed_effects);
    if opts.show_source {
        eprintln!("→ assembled program:");
        for l in src.lines() {
            eprintln!("    {l}");
        }
    }

    // 3) Parse + type-check. This is where a malicious body gets caught:
    // any effect not in `[allowed_effects]` shows up as an undeclared
    // effect on `fn tool` and the checker rejects it.
    let prog = load_program_from_str(&src).context("parse agent-generated source")?;
    let stages = canonicalize_program(&prog);

    // #132: every verification step below is an attestation producer
    // when `--store DIR` is set. Compute the StageId of the agent-
    // emitted `tool` fn once; open the log once. Subsequent emit
    // sites are content-addressed against this StageId so a later
    // `lex stage <id> --attestations` can answer "what evidence
    // exists for this exact body?".
    let tool_stage_id: Option<String> = stages.iter().find_map(|s| match s {
        Stage::FnDecl(fd) if fd.name == "tool" => stage_id(s),
        _ => None,
    });
    let att_log: Option<lex_vcs::AttestationLog> = match &opts.store_root {
        Some(root) => {
            let store = Store::open(root)
                .with_context(|| format!("opening store at {}", root.display()))?;
            Some(store.attestation_log()?)
        }
        None => None,
    };
    let model_for_attestation: Option<String> = match &opts.body_source {
        BodySource::Request(_) => Some(opts.model.clone()),
        _ => None,
    };

    if let Err(errs) = lex_types::check_program(&stages) {
        eprintln!("→ TYPE-CHECK REJECTED — tool not run.");
        for e in &errs {
            eprintln!("  {e}");
            if let lex_types::TypeError::EffectNotDeclared { effect, .. } = e {
                eprintln!(
                    "    (the body uses effect `{effect}` but the host only allows {:?})",
                    opts.allowed_effects
                );
            }
        }
        std::process::exit(2);
    }

    // 4) Compile + policy gate.
    let bc = compile_program(&stages);
    let mut policy = Policy::pure();
    policy.allow_effects = opts.allowed_effects.iter().cloned().collect();
    policy.allow_fs_read = opts.allow_fs_read.clone();
    policy.allow_net_host = opts.allow_net_host.clone();
    if let Err(violations) = check_policy(&bc, &policy) {
        eprintln!("→ POLICY REJECTED — tool not run.");
        for v in &violations {
            eprintln!("  {}", serde_json::to_string(v).unwrap_or_default());
        }
        std::process::exit(3);
    }

    // 4b) Spec proof. Strongest behavioral guarantee available pre-run:
    // a quantified property attached to `tool` is checked against the
    // emitted body before the tool ever executes on real inputs. SMT
    // (via Z3, when available) handles structural+integer cases;
    // randomized fallback covers the rest. Counterexamples abort with
    // exit 5; inconclusive aborts with 6 unless --spec-allow-inconclusive.
    if let Some(path) = opts.spec_file.as_ref() {
        let spec_text = fs::read_to_string(path)
            .with_context(|| format!("read spec file {}", path.display()))?;
        let spec = spec_checker::parse_spec(&spec_text).map_err(|e| anyhow!("spec parse: {e}"))?;
        if opts.show_source {
            eprintln!("→ checking spec `{}`…", spec.name);
        }
        let report = spec_checker::check_spec(&spec, &src, opts.spec_trials);

        // Emit the Spec attestation *before* the match below acts on
        // the verdict — Counterexample / strict Inconclusive both
        // exit, so we'd lose evidence on the failure path otherwise.
        // Failures are evidence too (#132 trust model).
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let result = match &report.status {
                spec_checker::ProofStatus::Proved => lex_vcs::AttestationResult::Passed,
                spec_checker::ProofStatus::Counterexample => {
                    let detail = report
                        .evidence
                        .counterexample
                        .as_ref()
                        .and_then(|c| serde_json::to_string(c).ok())
                        .map(|s| format!("counterexample: {s}"))
                        .unwrap_or_else(|| "counterexample".into());
                    lex_vcs::AttestationResult::Failed { detail }
                }
                spec_checker::ProofStatus::Inconclusive => {
                    lex_vcs::AttestationResult::Inconclusive {
                        detail: report
                            .evidence
                            .note
                            .clone()
                            .unwrap_or_else(|| "inconclusive".into()),
                    }
                }
            };
            let kind = lex_vcs::AttestationKind::Spec {
                spec_id: report.spec_id.clone(),
                method: lex_vcs::SpecMethod::Random,
                trials: Some(opts.spec_trials as usize),
            };
            emit_agent_tool_attestation(log, sid, kind, result, model_for_attestation.clone())?;
        }

        match report.status {
            spec_checker::ProofStatus::Proved => {
                if opts.show_source {
                    eprintln!(
                        "  spec proved ({} method, {} trials)",
                        report.evidence.method, report.evidence.trials
                    );
                }
            }
            spec_checker::ProofStatus::Counterexample => {
                eprintln!("→ SPEC COUNTEREXAMPLE — tool not run.");
                if let Some(cx) = &report.evidence.counterexample {
                    for (k, v) in cx {
                        eprintln!("  {k} = {v}");
                    }
                }
                if let Some(note) = &report.evidence.note {
                    eprintln!("  ({note})");
                }
                std::process::exit(5);
            }
            spec_checker::ProofStatus::Inconclusive => {
                eprintln!("→ SPEC INCONCLUSIVE — could not decide property.");
                if let Some(note) = &report.evidence.note {
                    eprintln!("  ({note})");
                }
                if !opts.spec_allow_inconclusive {
                    eprintln!("  (pass --spec-allow-inconclusive to ship anyway)");
                    std::process::exit(6);
                }
                eprintln!("  (continuing because --spec-allow-inconclusive is set)");
            }
        }
    }

    // 5) Run with a step-limit cap. This is the runtime DoS guard:
    // type-check rejects effects, max_steps rejects runaway compute.
    let bc = std::sync::Arc::new(bc);

    // 5-diff) Differential evaluation: if --diff-body is set, compile
    // the second body through the same gates and run both on each input
    // (single --input or every entry from --examples). Any output
    // divergence aborts with exit 7. Use case: detect regressions when
    // model v2's emission disagrees with v1's on inputs the host cares
    // about, without needing a full Spec proof.
    if let Some(diff_src) = opts.diff_body_source.as_ref() {
        let diff_body_text = match diff_src {
            BodySource::Literal(b) => b.clone(),
            BodySource::File(p) => fs::read_to_string(p)
                .with_context(|| format!("read diff body from {}", p.display()))?,
            BodySource::Request(_) => bail!(
                "--diff-body and --diff-body-file accept literal source; \
                 invoke Claude separately and pass the body in"
            ),
        };
        let diff_body_text = strip_code_fences(&diff_body_text);
        let diff_src = build_tool_program(&diff_body_text, &opts.allowed_effects);
        let prog_b = load_program_from_str(&diff_src).context("parse diff body")?;
        let stages_b = canonicalize_program(&prog_b);
        if let Err(errs) = lex_types::check_program(&stages_b) {
            eprintln!("→ DIFF BODY type-check rejected.");
            for e in &errs {
                eprintln!("  {e}");
            }
            std::process::exit(2);
        }
        let bc_b = compile_program(&stages_b);
        if let Err(violations) = check_policy(&bc_b, &policy) {
            eprintln!("→ DIFF BODY policy rejected.");
            for v in &violations {
                eprintln!("  {}", serde_json::to_string(v).unwrap_or_default());
            }
            std::process::exit(3);
        }
        let bc_b = std::sync::Arc::new(bc_b);

        // Inputs: --examples list or single --input.
        let inputs: Vec<String> = match opts.examples_file.as_ref() {
            Some(p) => load_examples(p)?.into_iter().map(|e| e.input).collect(),
            None => vec![opts.user_input.clone()],
        };

        if opts.show_source {
            eprintln!("→ comparing two bodies on {} input(s)…", inputs.len());
        }
        let mut diverged: Vec<(String, String, String)> = Vec::new();
        for input in &inputs {
            let out_a = run_tool_once(&bc, &policy, opts.max_steps, input)?;
            let out_b = run_tool_once(&bc_b, &policy, opts.max_steps, input)?;
            if out_a != out_b {
                diverged.push((input.clone(), out_a, out_b));
            }
        }
        // Emit a DiffBody attestation against the original tool's
        // StageId. `other_body_hash` is the SHA-256 of the second
        // body's source so re-running with the same pair dedups.
        // Failed attestation carries a summary of how many inputs
        // diverged.
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let other_body_hash = sha256_hex(diff_body_text.as_bytes());
            let result = if diverged.is_empty() {
                lex_vcs::AttestationResult::Passed
            } else {
                lex_vcs::AttestationResult::Failed {
                    detail: format!("{}/{} inputs diverged", diverged.len(), inputs.len()),
                }
            };
            let kind = lex_vcs::AttestationKind::DiffBody {
                other_body_hash,
                input_count: inputs.len(),
            };
            emit_agent_tool_attestation(log, sid, kind, result, model_for_attestation.clone())?;
        }

        if !diverged.is_empty() {
            eprintln!(
                "→ DIFFERENTIAL DIVERGENCE — {} of {} inputs differ.",
                diverged.len(),
                inputs.len()
            );
            for (input, a, b) in &diverged {
                eprintln!("  input={input:?}");
                eprintln!("    body A → {a:?}");
                eprintln!("    body B → {b:?}");
            }
            std::process::exit(7);
        }
        if opts.show_source {
            eprintln!("→ no divergence on {} input(s)", inputs.len());
        }
        // Print body A's output on the first input — single-shot mode.
        let chosen = inputs.first().cloned().unwrap_or_default();
        let out = run_tool_once(&bc, &policy, opts.max_steps, &chosen)?;
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let kind = lex_vcs::AttestationKind::SandboxRun {
                effects: opts.allowed_effects.iter().cloned().collect(),
            };
            emit_agent_tool_attestation(
                log,
                sid,
                kind,
                lex_vcs::AttestationResult::Passed,
                model_for_attestation.clone(),
            )?;
        }
        println!("{out}");
        return Ok(());
    }

    // 5a) If --examples is set, behavioral-verify before trusting the tool
    // for live traffic. Catches the well-typed-but-wrong-behavior gap:
    // the type system says what code touches; the examples say what it
    // should return. On any mismatch, exit 5 (distinct from 2/3/4).
    if let Some(path) = opts.examples_file.as_ref() {
        let raw_examples =
            fs::read(path).with_context(|| format!("read examples file {}", path.display()))?;
        let examples_file_hash = sha256_hex(&raw_examples);
        let examples: Vec<Example> = serde_json::from_slice(&raw_examples).with_context(|| {
            format!(
                "parse examples file {}; expected JSON array of {{input, expected}}",
                path.display()
            )
        })?;
        if opts.show_source {
            eprintln!("→ checking {} example(s)…", examples.len());
        }
        let mut failures: Vec<(usize, &Example, String)> = Vec::new();
        for (idx, ex) in examples.iter().enumerate() {
            let out = run_tool_once(&bc, &policy, opts.max_steps, &ex.input)?;
            if out != ex.expected {
                failures.push((idx, ex, out));
            }
        }

        // Emit Examples attestation regardless of pass/fail. Same
        // "failures are evidence too" rule as Spec.
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let result = if failures.is_empty() {
                lex_vcs::AttestationResult::Passed
            } else {
                lex_vcs::AttestationResult::Failed {
                    detail: format!("{}/{} examples mismatched", failures.len(), examples.len()),
                }
            };
            let kind = lex_vcs::AttestationKind::Examples {
                file_hash: examples_file_hash,
                count: examples.len(),
            };
            emit_agent_tool_attestation(log, sid, kind, result, model_for_attestation.clone())?;
        }

        if !failures.is_empty() {
            eprintln!(
                "→ EXAMPLES FAILED — tool not trusted ({} of {} mismatched).",
                failures.len(),
                examples.len()
            );
            for (i, ex, got) in &failures {
                eprintln!("  [{i}] input={:?}", ex.input);
                eprintln!("       expected={:?}", ex.expected);
                eprintln!("       got     ={got:?}");
            }
            std::process::exit(5);
        }
        if opts.show_source {
            eprintln!("→ examples passed: {}/{}", examples.len(), examples.len());
        }
    }

    // 5b) Single-shot run with the user_input. With --examples this
    // doubles as a sanity invocation; without examples it's the only run.
    let result = run_tool_once(&bc, &policy, opts.max_steps, &opts.user_input)?;

    // Emit a SandboxRun attestation tagging the effects the policy
    // actually allowed. `Passed` only — a runtime-error path
    // returns Err above and never reaches this point.
    if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
        let kind = lex_vcs::AttestationKind::SandboxRun {
            effects: opts.allowed_effects.iter().cloned().collect(),
        };
        emit_agent_tool_attestation(
            log,
            sid,
            kind,
            lex_vcs::AttestationResult::Passed,
            model_for_attestation.clone(),
        )?;
    }

    println!("{result}");
    Ok(())
}

#[derive(serde::Deserialize)]
pub(super) struct Example {
    pub(super) input: String,
    pub(super) expected: String,
}

pub(super) fn load_examples(path: &std::path::Path) -> Result<Vec<Example>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read examples file {}", path.display()))?;
    let cases: Vec<Example> = serde_json::from_str(&raw).with_context(|| {
        format!(
            "parse examples file {}; expected JSON array of {{input, expected}}",
            path.display()
        )
    })?;
    Ok(cases)
}

pub(super) fn run_tool_once(
    bc: &std::sync::Arc<lex_bytecode::Program>,
    policy: &Policy,
    max_steps: u64,
    input: &str,
) -> Result<String> {
    let handler = DefaultHandler::new(policy.clone()).with_program(std::sync::Arc::clone(bc));
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    vm.set_step_limit(max_steps);
    let result = match vm.call("tool", vec![Value::Str(input.to_string().into())]) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("step limit") {
                eprintln!("→ STEP-LIMIT EXCEEDED — tool aborted at {max_steps} steps.");
                eprintln!(
                    "  (raise with --max-steps; default {})",
                    default_max_steps()
                );
                std::process::exit(4);
            }
            // Runtime scope rejections (--allow-fs-read / --allow-net-host
            // / --allow-fs-write) surface as effect-handler errors. Exit 3
            // matches the static-policy gate so callers can branch cleanly:
            //   2 = type-check, 3 = policy (static or runtime), 4 = step-limit,
            //   5 = examples failed.
            if msg.contains("outside --allow-fs-read")
                || msg.contains("outside --allow-fs-write")
                || msg.contains("not in --allow-net-host")
            {
                eprintln!("→ POLICY REJECTED (runtime scope) — tool aborted.");
                eprintln!("  {e}");
                std::process::exit(3);
            }
            return Err(anyhow!("runtime: {e}"));
        }
    };
    Ok(match result {
        Value::Str(s) => s.to_string(),
        other => value_to_json_string(&other),
    })
}

pub(super) const fn default_max_steps() -> u64 {
    1_000_000
}

pub(super) fn parse_agent_tool_args(args: &[String]) -> Result<AgentToolOpts> {
    let mut allowed_effects: Vec<String> = Vec::new();
    let mut user_input: Option<String> = None;
    let mut body: Option<BodySource> = None;
    let mut api_key: Option<String> = None;
    let mut model = "claude-sonnet-4-6".to_string();
    let mut show_source = true;
    let mut max_steps: u64 = default_max_steps();
    let mut allow_fs_read: Vec<PathBuf> = Vec::new();
    let mut allow_net_host: Vec<String> = Vec::new();
    let mut examples_file: Option<PathBuf> = None;
    let mut spec_file: Option<PathBuf> = None;
    let mut spec_allow_inconclusive = false;
    let mut spec_trials: u32 = 1000;
    let mut diff_body: Option<BodySource> = None;
    let mut store_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow-effects" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-effects needs a value"))?;
                allowed_effects = v
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                i += 2;
            }
            "--allow-fs-read" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-fs-read needs a path"))?;
                allow_fs_read.push(PathBuf::from(v));
                i += 2;
            }
            "--allow-net-host" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-net-host needs a host"))?;
                allow_net_host.push(v.clone());
                i += 2;
            }
            "--input" => {
                user_input = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--input needs a value"))?
                        .clone(),
                );
                i += 2;
            }
            "--request" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--request needs a value"))?
                    .clone();
                if user_input.is_none() {
                    user_input = Some(v.clone());
                }
                body = Some(BodySource::Request(v));
                i += 2;
            }
            "--body" => {
                body = Some(BodySource::Literal(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--body needs a value"))?
                        .clone(),
                ));
                i += 2;
            }
            "--body-file" => {
                body = Some(BodySource::File(PathBuf::from(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--body-file needs a path"))?,
                )));
                i += 2;
            }
            "--api-key" => {
                api_key = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--api-key needs a value"))?
                        .clone(),
                );
                i += 2;
            }
            "--model" => {
                model = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--model needs a value"))?
                    .clone();
                i += 2;
            }
            "--max-steps" => {
                max_steps = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--max-steps needs a value"))?
                    .parse()
                    .context("--max-steps must be an integer")?;
                i += 2;
            }
            "--examples" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--examples needs a path"))?;
                examples_file = Some(PathBuf::from(v));
                i += 2;
            }
            "--spec" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--spec needs a path"))?;
                spec_file = Some(PathBuf::from(v));
                i += 2;
            }
            "--spec-allow-inconclusive" => {
                spec_allow_inconclusive = true;
                i += 1;
            }
            "--spec-trials" => {
                spec_trials = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--spec-trials needs an integer"))?
                    .parse()
                    .context("--spec-trials must be a u32")?;
                i += 2;
            }
            "--diff-body" => {
                diff_body = Some(BodySource::Literal(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--diff-body needs a value"))?
                        .clone(),
                ));
                i += 2;
            }
            "--diff-body-file" => {
                diff_body = Some(BodySource::File(PathBuf::from(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--diff-body-file needs a path"))?,
                )));
                i += 2;
            }
            "--store" => {
                store_root = Some(PathBuf::from(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow!("--store needs a path"))?,
                ));
                i += 2;
            }
            "--quiet" => {
                show_source = false;
                i += 1;
            }
            other => bail!("unknown agent-tool flag: {other}"),
        }
    }
    Ok(AgentToolOpts {
        allowed_effects,
        user_input: user_input.unwrap_or_default(),
        body_source: body.ok_or_else(|| {
            anyhow!("must pass --request '<query>', --body '<src>', or --body-file <path>")
        })?,
        api_key,
        model,
        show_source,
        max_steps,
        allow_fs_read,
        allow_net_host,
        examples_file,
        spec_file,
        spec_allow_inconclusive,
        spec_trials,
        diff_body_source: diff_body,
        store_root,
    })
}

pub(super) fn build_tool_program(body: &str, allowed_effects: &[String]) -> String {
    // Auto-import every std module so the agent can syntactically
    // reach any effect — the *signature* gates what's allowed. This
    // makes the demo land: a body using `io.read` resolves cleanly
    // to the io builtin, then the type checker rejects it with
    // "effect `io` not declared on `fn tool`" instead of a confusing
    // unknown-identifier error.
    let imports = [
        "import \"std.io\"    as io",
        "import \"std.net\"   as net",
        "import \"std.str\"   as str",
        "import \"std.int\"   as int",
        "import \"std.float\" as float",
        "import \"std.list\"  as list",
        "import \"std.json\"  as json",
        "import \"std.bytes\" as bytes",
    ]
    .join("\n");
    let effects = if allowed_effects.is_empty() {
        String::new()
    } else {
        format!("[{}] ", allowed_effects.join(", "))
    };
    // The tool's signature is fixed: input -> Str. The agent provides
    // only the body. Effects are declared from the host's allow-list
    // so any extra effect inside the body is an undeclared use.
    format!("{imports}\n\nfn tool(input :: Str) -> {effects}Str {{\n{body}\n}}\n")
}

pub(super) fn strip_code_fences(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix("```lex")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    // If the model wrapped the body in `fn tool(...) { ... }`, peel it down
    // to just the inner block so the template re-wraps it cleanly.
    if let Some((_, rest)) = t.split_once("fn tool(") {
        if let Some(open) = rest.find('{') {
            let after_brace = &rest[open + 1..];
            if let Some(close) = after_brace.rfind('}') {
                return after_brace[..close].trim().to_string();
            }
        }
    }
    t.to_string()
}

pub(super) fn call_claude_for_body(
    user_request: &str,
    allowed_effects: &[String],
    api_key: &str,
    model: &str,
) -> Result<String> {
    let effects_str = if allowed_effects.is_empty() {
        "(none)".to_string()
    } else {
        format!("[{}]", allowed_effects.join(", "))
    };
    let system = format!(
        r#"You are a code generator for the Lex programming language.

Output ONLY the body of:

    fn tool(input :: Str) -> {effects_str} Str {{ <YOUR BODY> }}

Imports already in scope: net, str, int, float, list, json.
Useful builtins:
  net.get(url :: Str) -> [net] Result[Str, Str]
  net.post(url, body) -> [net] Result[Str, Str]
  str.concat(a, b) -> Str          # use repeatedly to build a string
  str.split(s, sep) -> List[Str]
  str.contains(s, needle) -> Bool
  int.to_str(n :: Int) -> Str
  json.stringify(v) -> Str
  json.parse(s) -> Result[T, Str]

Hard constraints:
1. Only use effects from the set {effects_str}. ANY other effect (io.read,
   io.write, fs_read, fs_write, ...) will be rejected by the type
   checker before execution.
2. Output a single block-bodied expression (no `fn` declaration, no
   imports, no markdown fences). Begin directly with code.
3. Match Result with Ok/Err arms; never use a `.unwrap`.
4. Lex has no string interpolation — chain `str.concat(a, b)` calls.
"#
    );
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "system": system,
        "messages": [{ "role": "user", "content": user_request }],
    });
    let resp: serde_json::Value = ureq::post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send_json(body)
        .map_err(|e| anyhow!("claude api: {e}"))?
        .body_mut()
        .read_json::<serde_json::Value>()
        .context("decode claude response")?;
    let text = resp
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|item| {
                if item.get("type")?.as_str()? == "text" {
                    item.get("text")?.as_str().map(String::from)
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| anyhow!("claude response missing text content; got: {resp}"))?;
    Ok(text)
}
