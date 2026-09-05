//! `lex plan` (#307): cost-aware path planner over the call graph.

use super::*;

/// `lex plan --goal <fn> [--max-cost N] [--intent <id>] [--branch B] [--store DIR]`
/// (#307). Cost-aware path planner over the call graph. Advisory:
/// returns paths cheapest-first with a `fits` flag against the
/// effective cap; the agent (or downstream policy) decides which
/// path to apply.
pub(super) fn cmd_plan(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut goal: Option<String> = None;
    let mut max_cost: Option<u64> = None;
    let mut intent: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut root: Option<std::path::PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--goal" => goal = it.next().cloned(),
            "--max-cost" => {
                max_cost = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--max-cost needs N"))?
                        .parse()
                        .map_err(|e| anyhow!("--max-cost: {e}"))?,
                );
            }
            "--intent" => intent = it.next().cloned(),
            "--branch" => branch = it.next().cloned(),
            "--store" => {
                root = Some(std::path::PathBuf::from(
                    it.next().ok_or_else(|| anyhow!("--store needs a path"))?,
                ))
            }
            other => bail!("unexpected arg `{other}` for `lex plan`"),
        }
    }
    let goal = goal.ok_or_else(|| {
        anyhow!(
            "usage: lex plan --goal <fn> [--max-cost N] [--intent <id>] [--branch B] [--store DIR]"
        )
    })?;
    let root = root.unwrap_or_else(default_store_root);
    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    // If `--intent` is supplied, resolve its session id via the
    // IntentLog so the planner can consult `Store::session_budget`.
    let session_id = if let Some(intent_id) = &intent {
        let intent_log = lex_vcs::IntentLog::open(store.root())?;
        intent_log.get(intent_id)?.map(|i| i.session_id)
    } else {
        None
    };

    let plan = store
        .plan(&branch, &goal, max_cost, session_id.as_deref())
        .with_context(|| format!("planning paths from `{goal}` on branch `{branch}`"))?;
    let data = serde_json::to_value(&plan)?;
    acli::emit_or_text("plan", data, fmt, || {
        let cap = plan
            .effective_cap
            .map(|c| c.to_string())
            .unwrap_or_else(|| "(uncapped)".into());
        println!("plan from `{}` (effective cap: {}):", plan.goal, cap);
        if let (Some(sid), Some(r)) = (&plan.session_id, plan.remaining_budget) {
            println!("  session `{sid}`: remaining budget {r}");
        }
        if plan.paths.is_empty() {
            println!("  (no paths — goal not in the branch head's active set)");
        }
        for p in &plan.paths {
            let mark = if p.fits { "ok " } else { "no " };
            let effs = if p.effects.is_empty() {
                String::new()
            } else {
                format!(
                    " [{}]",
                    p.effects.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            println!(
                "  {mark} cost={} {}{}",
                p.total_cost,
                p.chain.join(" -> "),
                effs,
            );
        }
    });
    Ok(())
}
