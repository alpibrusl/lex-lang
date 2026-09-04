//! `lex replay`: re-run a recorded trace with node-level overrides.

use super::*;

pub(super) fn cmd_replay(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // usage: lex replay <run_id> <file> <fn> [args] [--override NODE=JSON]
    let mut overrides: indexmap::IndexMap<String, serde_json::Value> = indexmap::IndexMap::new();
    let mut policy = Policy::pure();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--override" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--override needs NODE=JSON"))?;
                let (node, json) = val
                    .split_once('=')
                    .ok_or_else(|| anyhow!("--override expects NODE=JSON"))?;
                let v: serde_json::Value = serde_json::from_str(json)
                    .with_context(|| format!("--override value must be JSON: {json}"))?;
                overrides.insert(node.to_string(), v);
                i += 2;
            }
            "--allow-effects" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--allow-effects needs a value"))?;
                policy.allow_effects = val
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect::<BTreeSet<_>>();
                i += 2;
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    let _orig_run_id = positional
        .first()
        .ok_or_else(|| anyhow!("usage: lex replay <run_id> <file> <fn> [args]"))?;
    let path = positional.get(1).ok_or_else(|| anyhow!("missing <file>"))?;
    let func = positional.get(2).ok_or_else(|| anyhow!("missing <fn>"))?;

    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        let arr: Vec<serde_json::Value> = errs
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let data = serde_json::json!({ "phase": "type-check", "errors": arr });
        acli::emit_or_text("replay", data, fmt, || {
            for e in &errs {
                if let Ok(j) = serde_json::to_string(e) {
                    eprintln!("{j}");
                }
            }
        });
        std::process::exit(2);
    }
    let bc = compile_program(&stages);
    if let Err(violations) = check_policy(&bc, &policy) {
        let arr: Vec<serde_json::Value> = violations
            .iter()
            .map(|v| serde_json::to_value(v).unwrap())
            .collect();
        let data = serde_json::json!({ "phase": "policy", "violations": arr });
        acli::emit_or_text("replay", data, fmt, || {
            for v in &violations {
                if let Ok(j) = serde_json::to_string(v) {
                    eprintln!("{j}");
                }
            }
        });
        std::process::exit(3);
    }

    let recorder = lex_trace::Recorder::new().with_overrides(overrides);
    let handle = recorder.handle();
    let bc = std::sync::Arc::new(bc);
    let handler = DefaultHandler::new(policy).with_program(std::sync::Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.set_tracer(Box::new(recorder));

    let vargs: Vec<Value> = positional[3..]
        .iter()
        .map(|a| {
            let v: serde_json::Value =
                serde_json::from_str(a).with_context(|| format!("arg `{a}` must be JSON"))?;
            Ok(json_to_value(&v))
        })
        .collect::<Result<Vec<_>>>()?;

    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let result = vm.call(func, vargs);
    let ended = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let store = lex_store::Store::open(default_store_root())?;
    let (root_out, root_err) = match &result {
        Ok(v) => (Some(value_to_json(v)), None),
        Err(e) => (None, Some(format!("{e}"))),
    };
    let tree = handle.finalize(
        func.clone(),
        serde_json::Value::Null,
        root_out,
        root_err,
        started,
        ended,
    );
    let new_run_id = store.save_trace(&tree)?;
    if !matches!(fmt, OutputFormat::Json) {
        eprintln!("trace saved: {new_run_id}");
    }
    let r = result.map_err(|e| anyhow!("runtime: {e}"))?;
    let data = serde_json::json!({
        "result": value_to_json(&r),
        "trace_id": new_run_id,
    });
    acli::emit_or_text("replay", data, fmt, || {
        println!("{}", value_to_json_string(&r))
    });
    Ok(())
}
