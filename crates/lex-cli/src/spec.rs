//! `lex conformance` and `lex spec`: conformance-suite and spec-checker drivers.

use super::*;

pub(super) fn cmd_conformance(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let dir = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex conformance <dir>"))?;
    let report = conformance::run_directory(dir).context("reading conformance directory")?;
    let total = report.total();
    let passed_n = report.passed.len();
    let failed: Vec<serde_json::Value> = report
        .failed
        .iter()
        .map(|(n, w)| serde_json::json!({ "name": n, "reason": w }))
        .collect();
    let data = serde_json::json!({
        "passed": &report.passed,
        "failed": failed,
        "total": total,
        "passed_count": passed_n,
        "ok": report.ok(),
    });
    acli::emit_or_text("conformance", data, fmt, || {
        for name in &report.passed {
            println!("PASS  {name}");
        }
        for (name, why) in &report.failed {
            println!("FAIL  {name}: {why}");
        }
        println!();
        println!("{}/{} passed", passed_n, total);
    });
    if report.ok() {
        Ok(())
    } else {
        std::process::exit(4);
    }
}

pub(super) fn cmd_spec(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex spec {{check|smt}} ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "check" => {
            let mut spec_path: Option<&String> = None;
            let mut src_path: Option<&String> = None;
            let mut trials: u32 = 1000;
            let mut store_root: Option<PathBuf> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--source" => {
                        src_path = rest.get(i + 1);
                        i += 2;
                    }
                    "--trials" => {
                        trials = rest
                            .get(i + 1)
                            .and_then(|s| s.parse().ok())
                            .ok_or_else(|| anyhow!("--trials needs a u32"))?;
                        i += 2;
                    }
                    "--store" => {
                        store_root = rest.get(i + 1).map(PathBuf::from);
                        i += 2;
                    }
                    _ if spec_path.is_none() => {
                        spec_path = Some(&rest[i]);
                        i += 1;
                    }
                    other => bail!("unexpected arg `{other}`"),
                }
            }
            let spec_path =
                spec_path.ok_or_else(|| anyhow!("usage: lex spec check <spec> --source <file>"))?;
            let src_path = src_path.ok_or_else(|| anyhow!("--source <file> required"))?;
            let spec_src = read_source(spec_path)?;
            let lex_src = read_source(src_path)?;
            let spec =
                spec_checker::parse_spec(&spec_src).map_err(|e| anyhow!("spec parse: {e}"))?;
            let r = spec_checker::check_spec(&spec, &lex_src, trials);

            // #132: when --store is provided, emit a Spec attestation
            // tied to the StageId of the function the spec targets.
            // The attestation captures the verification result
            // (passed / failed-with-counterexample / inconclusive)
            // so a downstream `lex blame --with-evidence` or
            // `GET /v1/stage/<id>/attestations` answers "has this
            // stage ever been spec-checked?" without re-running.
            //
            // No-ops if `--store` is absent or the source doesn't
            // contain a fn matching `spec.name` (the typical case
            // is a spec referring to a fn that *is* in the source).
            if let Some(root) = &store_root {
                if let Some(target_stage_id) = find_stage_id_for_fn(&lex_src, &spec.name) {
                    record_spec_attestation(root, &target_stage_id, &spec.name, &r, trials)?;
                }
            }

            let data = serde_json::to_value(&r)?;
            acli::emit_or_text("spec", data.clone(), fmt, || {
                println!("{}", serde_json::to_string_pretty(&data).unwrap());
            });
            // Exit code: 0 proved, 5 counterexample, 6 inconclusive.
            match r.status {
                spec_checker::ProofStatus::Proved => Ok(()),
                spec_checker::ProofStatus::Counterexample => std::process::exit(5),
                spec_checker::ProofStatus::Inconclusive => std::process::exit(6),
            }
        }
        "smt" => {
            let path = rest
                .first()
                .ok_or_else(|| anyhow!("usage: lex spec smt <spec>"))?;
            let spec_src = read_source(path)?;
            let spec =
                spec_checker::parse_spec(&spec_src).map_err(|e| anyhow!("spec parse: {e}"))?;
            let smt = spec_checker::to_smtlib(&spec);
            let data = serde_json::json!({ "smt_lib": &smt });
            acli::emit_or_text("spec", data, fmt, || print!("{smt}"));
            Ok(())
        }
        other => bail!("unknown `lex spec` subcommand: {other}"),
    }
}
