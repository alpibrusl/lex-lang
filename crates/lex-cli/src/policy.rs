//! `lex policy`: producer blocks, session budgets and required-attestation rules.

use super::*;

/// `lex policy {block-producer|unblock-producer|list}` — manage
/// the local trust policy at `<store>/policy.json` (#181). The
/// list is consulted at attestation-read time: producers on it
/// keep their attestations in the log (audit trail intact) but
/// the activity feed and other consumers tag those rows
/// `blocked`. Enforcement is local; nothing is mutated in the
/// attestation log itself.
pub(super) fn cmd_policy(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex policy {{block-producer <name> --reason \"...\" | unblock-producer <name> | \
         require-attestation <kind> [--when-effects e1,e2,...] | unrequire-attestation <kind> | \
         session-budget {{set-default <N> | set <id> <N> | unbounded <id> | clear <id> | clear-default}} | \
         list | show}} [--store DIR]"
    ))?;
    let rest = &args[1..];
    match sub.as_str() {
        "block-producer" => cmd_policy_block(fmt, rest),
        "unblock-producer" => cmd_policy_unblock(fmt, rest),
        "require-attestation" => cmd_policy_require_attestation(fmt, rest),
        "unrequire-attestation" => cmd_policy_unrequire_attestation(fmt, rest),
        "session-budget" => cmd_policy_session_budget(fmt, rest),
        // `show` is the new name; `list` is kept as an alias for the
        // pre-#245 muscle memory.
        "list" | "show" => cmd_policy_show(fmt, rest),
        other => bail!("unknown `lex policy` subcommand: {other}"),
    }
}

/// `lex policy session-budget <subcmd>` — manage
/// `policy.session_budgets` (#292 slices 2 + 3).
pub(super) fn cmd_policy_session_budget(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| {
        anyhow!(
            "usage: lex policy session-budget {{set-default <N> | set <id> <N> | \
         unbounded <id> | clear <id> | clear-default}} [--store DIR]"
        )
    })?;
    let (root, rest, _, _) = parse_store_flag(&args[1..]);
    let mut policy = lex_store::policy::load(&root)
        .map_err(|e| anyhow!("loading policy.json: {e}"))?
        .unwrap_or_default();
    let action = match sub.as_str() {
        "set-default" => {
            let n: u64 = rest
                .first()
                .ok_or_else(|| anyhow!("usage: lex policy session-budget set-default <N>"))?
                .parse()
                .map_err(|e| anyhow!("invalid N: {e}"))?;
            policy.session_budgets.default_cap = Some(n);
            format!("set default_cap to {n}")
        }
        "set" => {
            let id = rest
                .first()
                .ok_or_else(|| anyhow!("usage: lex policy session-budget set <session_id> <N>"))?
                .clone();
            let n: u64 = rest
                .get(1)
                .ok_or_else(|| anyhow!("usage: lex policy session-budget set <session_id> <N>"))?
                .parse()
                .map_err(|e| anyhow!("invalid N: {e}"))?;
            policy.session_budgets.overrides.insert(id.clone(), Some(n));
            format!("set override `{id}` to {n}")
        }
        "unbounded" => {
            let id = rest
                .first()
                .ok_or_else(|| anyhow!("usage: lex policy session-budget unbounded <session_id>"))?
                .clone();
            policy.session_budgets.overrides.insert(id.clone(), None);
            format!("set override `{id}` to unbounded")
        }
        "clear" => {
            let id = rest
                .first()
                .ok_or_else(|| anyhow!("usage: lex policy session-budget clear <session_id>"))?;
            policy.session_budgets.overrides.remove(id);
            format!("cleared override `{id}`")
        }
        "clear-default" => {
            policy.session_budgets.default_cap = None;
            "cleared default_cap".into()
        }
        other => bail!("unknown `session-budget` subcommand: {other}"),
    };
    lex_store::policy::save(&root, &policy).map_err(|e| anyhow!("writing policy.json: {e}"))?;
    let action_for_text = action.clone();
    let data = serde_json::json!({
        "action": action,
        "session_budgets": serde_json::to_value(&policy.session_budgets)?,
    });
    acli::emit_or_text("policy", data, fmt, move || {
        println!("policy.session_budgets: {action_for_text}");
    });
    Ok(())
}

pub(super) fn cmd_policy_block(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut name: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--reason" => {
                reason = rest.get(i + 1).cloned();
                i += 2;
            }
            other if name.is_none() && !other.starts_with("--") => {
                name = Some(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let name =
        name.ok_or_else(|| anyhow!("usage: lex policy block-producer <name> --reason \"...\""))?;
    let reason = reason.ok_or_else(|| anyhow!("lex policy block-producer: --reason required"))?;
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let was_already_blocked = policy.is_blocked(&name);
    policy.block(name.clone(), reason.clone(), now);
    lex_store::policy::save(&root, &policy)
        .with_context(|| format!("writing policy.json at {}", root.display()))?;

    let data = serde_json::json!({
        "tool": &name,
        "reason": &reason,
        "blocked_at": now,
        "newly_blocked": !was_already_blocked,
    });
    let name_for_text = name.clone();
    acli::emit_or_text("policy", data, fmt, move || {
        if was_already_blocked {
            println!("(already blocked) {name_for_text}");
        } else {
            println!("→ blocked producer `{name_for_text}`");
        }
    });
    Ok(())
}

pub(super) fn cmd_policy_unblock(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let name = rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow!("usage: lex policy unblock-producer <name>"))?
        .clone();
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let removed = policy.unblock(&name);
    if removed {
        lex_store::policy::save(&root, &policy)
            .with_context(|| format!("writing policy.json at {}", root.display()))?;
    }
    let data = serde_json::json!({
        "tool": &name,
        "was_blocked": removed,
    });
    let name_for_text = name.clone();
    acli::emit_or_text("policy", data, fmt, move || {
        if removed {
            println!("→ unblocked producer `{name_for_text}`");
        } else {
            println!("(not blocked) {name_for_text}");
        }
    });
    Ok(())
}

/// `lex policy show` (formerly `lex policy list`) — render every
/// active rule in `policy.json`. Covers both the negative
/// `blocked_producers` gate (#181) and the positive
/// `required_attestations` gate (#245).
pub(super) fn cmd_policy_show(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, _rest, _, _) = parse_store_flag(args);
    let policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let required_json: Vec<serde_json::Value> = policy
        .required_attestations
        .iter()
        .map(|r| match &r.when {
            lex_store::policy::AttestationCondition::Always => serde_json::json!({
                "kind": r.kind.tag(),
                "when": "always",
            }),
            lex_store::policy::AttestationCondition::EffectsIntersect(effects) => {
                serde_json::json!({
                    "kind": r.kind.tag(),
                    "when": "effects_intersect",
                    "effects": effects.iter().collect::<Vec<_>>(),
                })
            }
        })
        .collect();
    let data = serde_json::json!({
        "blocked_producers": &policy.blocked_producers,
        // `count` is the pre-#245 key (count of blocked producers).
        // Kept under that name so external `lex policy list --output
        // json` consumers don't break; new `blocked_count` /
        // `required_count` are the explicit, namespaced versions.
        "count": policy.blocked_producers.len(),
        "blocked_count": policy.blocked_producers.len(),
        "required_attestations": required_json,
        "required_count": policy.required_attestations.len(),
    });
    let blocked = policy.blocked_producers.clone();
    let required = policy.required_attestations.clone();
    acli::emit_or_text("policy", data, fmt, move || {
        println!("# blocked producers");
        if blocked.is_empty() {
            println!("(none)");
        } else {
            for p in &blocked {
                println!("{}\tsince={}\treason={}", p.tool, p.blocked_at, p.reason);
            }
        }
        println!("\n# required attestations");
        if required.is_empty() {
            println!("(none)");
        } else {
            for r in &required {
                match &r.when {
                    lex_store::policy::AttestationCondition::Always => {
                        println!("{}\twhen=always", r.kind.tag());
                    }
                    lex_store::policy::AttestationCondition::EffectsIntersect(effects) => {
                        let list = effects.iter().cloned().collect::<Vec<_>>().join(",");
                        println!("{}\twhen=effects_intersect({list})", r.kind.tag());
                    }
                }
            }
        }
    });
    Ok(())
}

/// `lex policy require-attestation <kind> [--when-effects e1,e2,...]`
/// (#245). Adds a positive gate rule. Without `--when-effects`, the
/// rule fires on every op (`AttestationCondition::Always`); with it,
/// the rule only fires when the op's declared effect set intersects
/// the listed effects.
pub(super) fn cmd_policy_require_attestation(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut kind_str: Option<String> = None;
    let mut effects: Option<std::collections::BTreeSet<String>> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--when-effects" => {
                let v = rest
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--when-effects needs a comma-separated list"))?;
                effects = Some(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
                i += 2;
            }
            other if kind_str.is_none() && !other.starts_with("--") => {
                kind_str = Some(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let kind_str = kind_str.ok_or_else(|| {
        anyhow!(
            "usage: lex policy require-attestation <kind> [--when-effects e1,e2,...]\n\
         supported kinds: type_check, spec, sandbox_run, examples, diff_body, effect_audit"
        )
    })?;
    let kind = lex_store::policy::RequiredAttestationKind::from_tag(&kind_str)
        .ok_or_else(|| anyhow!("unknown attestation kind `{kind_str}`"))?;
    let when = match effects {
        Some(set) => lex_store::policy::AttestationCondition::EffectsIntersect(set),
        None => lex_store::policy::AttestationCondition::Always,
    };
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let added = policy.require_attestation(kind, when.clone());
    lex_store::policy::save(&root, &policy)
        .with_context(|| format!("writing policy.json at {}", root.display()))?;

    let when_json = match &when {
        lex_store::policy::AttestationCondition::Always => serde_json::json!({"always": null}),
        lex_store::policy::AttestationCondition::EffectsIntersect(set) => {
            serde_json::json!({"effects_intersect": set.iter().collect::<Vec<_>>()})
        }
    };
    let data = serde_json::json!({
        "kind": kind.tag(),
        "when": when_json,
        "newly_added": added,
    });
    let kind_tag = kind.tag();
    acli::emit_or_text("policy", data, fmt, move || {
        if added {
            println!("→ require attestation `{kind_tag}`");
        } else {
            println!("(already required) {kind_tag}");
        }
    });
    Ok(())
}

/// `lex policy unrequire-attestation <kind>` (#245). Removes every
/// rule with the given kind. To narrow a rule (Always → effects),
/// unrequire then re-require.
pub(super) fn cmd_policy_unrequire_attestation(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let kind_str = rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow!("usage: lex policy unrequire-attestation <kind>"))?
        .clone();
    let kind = lex_store::policy::RequiredAttestationKind::from_tag(&kind_str)
        .ok_or_else(|| anyhow!("unknown attestation kind `{kind_str}`"))?;
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let removed = policy.unrequire_attestation(kind);
    if removed > 0 {
        lex_store::policy::save(&root, &policy)
            .with_context(|| format!("writing policy.json at {}", root.display()))?;
    }
    let data = serde_json::json!({
        "kind": kind.tag(),
        "removed": removed,
    });
    let kind_tag = kind.tag();
    acli::emit_or_text("policy", data, fmt, move || {
        if removed > 0 {
            println!("→ removed {removed} rule(s) for `{kind_tag}`");
        } else {
            println!("(no rules) {kind_tag}");
        }
    });
    Ok(())
}

pub(super) fn attestation_result_tag(r: &lex_vcs::AttestationResult) -> &'static str {
    use lex_vcs::AttestationResult::*;
    match r {
        Passed => "passed",
        Failed { .. } => "failed",
        Inconclusive { .. } => "inconclusive",
    }
}

/// Accept either Unix epoch seconds (a u64) or `YYYY-MM-DD`. The
/// date form resolves to start-of-day UTC. Returns `None` on a
/// shape we don't recognize so the caller can surface a friendly
/// usage error.
pub(super) fn parse_since(s: &str) -> Option<u64> {
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    if y < 1970 {
        return None;
    }

    let mut days: i64 = 0;
    for yr in 1970..y {
        let yd = if (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0 {
            366
        } else {
            365
        };
        days += yd;
    }
    let leap_year = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [
        31,
        if leap_year { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mi = (m - 1) as usize;
    if d > mdays[mi] as u32 {
        return None;
    }
    days += mdays.iter().take(mi).sum::<i64>();
    days += (d - 1) as i64;
    Some((days as u64) * 86_400)
}
