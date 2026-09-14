//! Regenerators for `lex op replay` (#836 G3): the model-call half of
//! replay-as-verification, which lives outside the store on purpose
//! (the store owns the deterministic comparison; the model call is the
//! harness's). Two built-in regenerators are provided so replay works
//! out of the box:
//!
//! * [`regenerate_ollama`] — a local Ollama daemon (`OLLAMA_HOST` or
//!   `http://localhost:11434`). No API key, fully local.
//! * [`regenerate_cmd`] — any external command: the [`ReplayRequest`]
//!   JSON is piped to its stdin, the regenerated Lex source read from
//!   its stdout. This is the provider-agnostic seam — wire opencode,
//!   an Anthropic call, anything.
//!
//! Both return Lex source for the target function, which the caller
//! parses and feeds to `Store::replay_compare`.

use anyhow::{anyhow, bail, Context, Result};
use lex_store::ReplayRequest;

/// Build the regeneration prompt from a replay request: the parent
/// program as context, the recorded intent as the ask, and the target
/// signature as the interface to implement.
pub fn regen_prompt(req: &ReplayRequest) -> String {
    let mut p = String::new();
    p.push_str("You are regenerating a single Lex function from its recorded intent.\n\n");
    p.push_str(
        "Lex syntax: functions are expression-bodied — the last expression is the \
         return value, with NO `return` keyword and NO trailing semicolons. Type \
         annotations use `::` (e.g. `x :: Int`). Local bindings are `let name := value`. \
         Pattern matching is `match e { Pat => expr, ... }`. Example:\n\
         fn add(a :: Int, b :: Int) -> Int { a + b }\n\n",
    );
    if !req.parent_program.trim().is_empty() {
        p.push_str("Existing program (context — do NOT repeat it in your output):\n");
        p.push_str(&req.parent_program);
        p.push_str("\n\n");
    }
    match &req.prompt {
        Some(prompt) => {
            p.push_str("Intent (what was asked):\n");
            p.push_str(prompt);
            p.push_str("\n\n");
        }
        None => p.push_str("Intent: (none recorded — reproduce the function faithfully)\n\n"),
    }
    if let Some(sig) = &req.target_signature {
        p.push_str("Regenerate exactly this function, with this signature:\n");
        p.push_str(sig);
        p.push('\n');
    }
    p.push_str(
        "\nOutput ONLY the Lex source of that one function — no prose, \
         no markdown fences, no other definitions.\n",
    );
    p
}

/// Regenerate via a local Ollama daemon. `model` defaults to the
/// recorded model's name when present, else a caller default.
pub fn regenerate_ollama(req: &ReplayRequest, model: &str) -> Result<String> {
    let host = std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:11434".to_string());
    let url = format!("{host}/api/generate");
    let payload = serde_json::json!({
        "model": model,
        "prompt": regen_prompt(req),
        "stream": false,
        // Deterministic decoding: replay measures reproducibility, so
        // sampling noise would only muddy the signal.
        "think": false,
        "options": { "temperature": 0 },
    });
    let body = serde_json::to_string(&payload)?;
    let resp = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|e| anyhow!("POST {url}: {e} (is `ollama serve` running?)"))?;
    let v: serde_json::Value = resp
        .into_body()
        .read_json()
        .map_err(|e| anyhow!("decoding ollama response: {e}"))?;
    let text = v
        .get("response")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow!("ollama response had no `response` field: {v}"))?;
    Ok(strip_code_fences(text))
}

/// Regenerate via an external command: pipe the request JSON to its
/// stdin, read the Lex source from its stdout. Runs through `sh -c` so
/// the caller can pass a full command line.
pub fn regenerate_cmd(req: &ReplayRequest, cmd: &str) -> Result<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let request_json = serde_json::to_string_pretty(req)?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning regenerate-cmd `{cmd}`"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no stdin on regenerate-cmd child"))?
        .write_all(request_json.as_bytes())
        .context("writing request to regenerate-cmd stdin")?;
    let out = child.wait_with_output().context("waiting on regenerate-cmd")?;
    if !out.status.success() {
        bail!(
            "regenerate-cmd `{cmd}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(strip_code_fences(&String::from_utf8_lossy(&out.stdout)))
}

/// Strip a leading/trailing markdown code fence if the model wrapped
/// its output in one, and trim surrounding whitespace. Keeps the inner
/// source verbatim.
fn strip_code_fences(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    // Drop the optional language tag on the opening fence line.
    let rest = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => rest,
    };
    let rest = rest.strip_suffix("```").unwrap_or(rest);
    rest.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_fenced_output() {
        assert_eq!(strip_code_fences("```lex\nfn f() -> Int { 1 }\n```"), "fn f() -> Int { 1 }");
        assert_eq!(strip_code_fences("```\nfn f() -> Int { 1 }\n```"), "fn f() -> Int { 1 }");
        assert_eq!(strip_code_fences("  fn f() -> Int { 1 }  "), "fn f() -> Int { 1 }");
    }
}
