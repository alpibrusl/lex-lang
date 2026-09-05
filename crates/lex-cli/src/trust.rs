//! `lex producer-trust` and `lex keygen`: producer trust scores, keyrings, and signing keys.

use super::*;

/// `lex producer-trust recompute --tool <id> [--window N] [--granted-by ACTOR] [--store DIR]`
/// (#293). Walks the attestation log filtered by `produced_by.tool
/// == <id>`, computes `passed/total` over the last `window`
/// records, and emits a fresh `ProducerTrust` attestation. The
/// `required_attestations` gate consults the latest score per
/// tool to apply trust-based waivers.
pub(super) fn cmd_producer_trust(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| {
        anyhow!(
            "usage: lex producer-trust <recompute|keyring> ... \
             (recompute --tool <id>; keyring [--min-trust N] [--out FILE])"
        )
    })?;
    match sub.as_str() {
        "recompute" => {} // handled inline below
        "keyring" => return cmd_producer_trust_keyring(fmt, &args[1..]),
        other => bail!("unknown `lex producer-trust` subcommand: {other}"),
    }
    let (root, rest, _, _) = parse_store_flag(&args[1..]);
    let mut tool: Option<String> = None;
    let mut window: usize = 1000;
    let mut granted_by: String = whoami_id();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tool" => {
                tool = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--tool needs an id"))?
                        .clone(),
                );
            }
            "--window" => {
                window = it
                    .next()
                    .ok_or_else(|| anyhow!("--window needs N"))?
                    .parse()
                    .map_err(|e| anyhow!("--window: {e}"))?;
            }
            "--granted-by" => {
                granted_by = it
                    .next()
                    .ok_or_else(|| anyhow!("--granted-by needs an actor"))?
                    .clone();
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let tool =
        tool.ok_or_else(|| anyhow!("usage: lex producer-trust recompute --tool <id> ..."))?;

    let store = Store::open(&root)?;
    let result = store.recompute_producer_trust(&tool, window, &granted_by)?;
    let env = match &result {
        Some(att_id) => serde_json::json!({
            "tool": &tool,
            "window": window,
            "granted_by": &granted_by,
            "attestation_id": att_id,
            "ok": true,
        }),
        None => serde_json::json!({
            "tool": &tool,
            "window": window,
            "granted_by": &granted_by,
            "ok": false,
            "reason": "no attestations from this tool to score",
        }),
    };
    let env_for_text = env.clone();
    let tool_for_text = tool.clone();
    acli::emit_or_text("producer-trust", env, fmt, move || {
        if env_for_text["ok"] == true {
            println!(
                "recomputed trust for `{tool_for_text}` → attestation_id={}",
                env_for_text["attestation_id"].as_str().unwrap_or("?")
            );
        } else {
            println!(
                "no trust recompute: {}",
                env_for_text["reason"].as_str().unwrap_or("?")
            );
        }
    });
    Ok(())
}

/// `lex producer-trust keyring [--store DIR] [--min-trust N] [--out FILE]`:
/// export a capsule trusted-keys keyring of every producer whose live
/// `ProducerTrust` score is ≥ N thousandths (default 700). The producer id is
/// the publisher's signing key in the capsule model, so this turns *earned*
/// trust into the `{"trusted":[…]}` file that
/// `lex-os capsule install --trusted-keys` consumes — authorization derived
/// from a publisher's track record, not a hand-pinned allowlist. Prints the
/// keyring to stdout when `--out` is omitted, so it can be piped.
pub(super) fn cmd_producer_trust_keyring(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut min_trust: u32 = 700;
    let mut out: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--min-trust" => {
                min_trust = it
                    .next()
                    .ok_or_else(|| anyhow!("--min-trust needs N (0..=1000)"))?
                    .parse()
                    .map_err(|e| anyhow!("--min-trust: {e}"))?;
            }
            "--out" => {
                out = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--out needs a path"))?
                        .clone(),
                );
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let store = Store::open(&root)?;
    let scores = store.live_producer_trust_scores()?;
    let trusted: Vec<String> = scores
        .iter()
        .filter(|(_, s)| **s >= min_trust)
        .map(|(k, _)| k.clone())
        .collect();
    let keyring = serde_json::json!({ "trusted": trusted });
    let pretty = serde_json::to_string_pretty(&keyring)?;
    if let Some(path) = &out {
        std::fs::write(path, format!("{pretty}\n")).with_context(|| format!("writing {path}"))?;
    }
    let count = trusted.len();
    let data = serde_json::json!({
        "min_trust_thousandths": min_trust,
        "trusted_count": count,
        "trusted": trusted,
        "written_to": out,
    });
    acli::emit_or_text("producer-trust.keyring", data, fmt, move || match &out {
        Some(p) => eprintln!("wrote {count} trusted key(s) (score ≥ {min_trust}) to {p}"),
        None => println!("{pretty}"),
    });
    Ok(())
}

/// Best-effort identity for `--granted-by`. Reads `$USER`
/// (set on Unix login shells) or falls back to `"unknown"`.
pub(super) fn whoami_id() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LEX_TEA_USER"))
        .unwrap_or_else(|_| "unknown".into())
}

/// `lex keygen` — print a fresh Ed25519 keypair as hex.
///
/// Default text output is two lines:
///
///   `public_key  <hex>`
///   `secret_key  <hex>`
///
/// JSON output emits `{ "public_key": "...", "secret_key": "..." }`.
/// The secret key is printed once and never persisted by Lex itself —
/// the caller is responsible for storing it (env var, secret manager,
/// hardware token, etc.).
pub(super) fn cmd_keygen(fmt: &OutputFormat, _args: &[String]) -> Result<()> {
    let kp = lex_vcs::Keypair::generate().map_err(|e| anyhow!("keygen: {e}"))?;
    let data = serde_json::json!({
        "public_key": kp.public_hex(),
        "secret_key": kp.secret_hex(),
    });
    let pk = kp.public_hex();
    let sk = kp.secret_hex();
    acli::emit_or_text("keygen", data, fmt, move || {
        println!("public_key  {pk}");
        println!("secret_key  {sk}");
    });
    Ok(())
}

/// Resolve a signing key from the CLI flag, then env var, then None.
/// Returns `Ok(None)` if neither is set so the caller can decide
/// whether unsigned publish is allowed.
pub(super) fn resolve_signing_key(flag_value: Option<&str>) -> Result<Option<lex_vcs::Keypair>> {
    let hex_str = match flag_value {
        Some(v) => Some(v.to_string()),
        None => std::env::var("LEX_SIGNING_KEY").ok(),
    };
    match hex_str {
        Some(s) if !s.is_empty() => {
            let kp = lex_vcs::Keypair::from_secret_hex(s.trim()).map_err(|e| {
                anyhow!(
                    "invalid signing key (hex): {e}. \
                     Expected 64 hex chars from `lex keygen`."
                )
            })?;
            Ok(Some(kp))
        }
        _ => Ok(None),
    }
}

/// Apply `--require-signed` / `--trusted-key` policy to a stage's
/// metadata. Returns `Ok(())` if the policy permits the stage:
///
/// * If `require_signed` is true and `metadata.signature` is `None`,
///   error.
/// * If `trusted_key` is set, the signature must be present, must
///   verify, and the public key must match the trusted key.
/// * If `require_signed` is true and a signature is present, the
///   signature must verify.
/// * Otherwise (no flags set, present-but-not-required signature),
///   we still verify a present signature so that a corrupted record
///   surfaces clearly rather than silently passing.
pub(super) fn verify_metadata_signature(
    meta: &lex_store::Metadata,
    require_signed: bool,
    trusted_key: Option<&str>,
) -> Result<()> {
    match &meta.signature {
        None => {
            if require_signed {
                bail!(
                    "stage `{}` is not signed (--require-signed/--trusted-key was set)",
                    meta.stage_id
                );
            }
            Ok(())
        }
        Some(sig) => {
            lex_vcs::verify_stage_id(&meta.stage_id, sig).map_err(|e| {
                anyhow!(
                    "signature on stage `{}` failed verification: {e}",
                    meta.stage_id
                )
            })?;
            if let Some(trusted) = trusted_key {
                if !sig.public_key.eq_ignore_ascii_case(trusted) {
                    bail!(
                        "stage `{}` is signed by `{}`, not by trusted key `{}`",
                        meta.stage_id,
                        sig.public_key,
                        trusted
                    );
                }
            }
            Ok(())
        }
    }
}
