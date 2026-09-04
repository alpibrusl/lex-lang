//! `approval` effect: the `ApprovalSink` trait, its stdin / null implementations, and the `approval.*` dispatch gated by `--allow-approval`.

use super::*;

/// Host boundary for the `[approval]` effect. `request` blocks until an
/// operator answers — `Ok(answer)` on approve, `Err(reason)` on deny or
/// timeout. Implementations decide what "blocks" means (a stdin prompt,
/// an HTTP long-poll against a dashboard, ...); the effect handler only
/// needs the synchronous result.
pub trait ApprovalSink: Send {
    fn request(&self, scope: &str, reason: &str) -> Result<String, String>;
}

/// Default sink: `approval.request` is granted by the type/effect
/// system but there's no operator to ask, so every call is refused.
/// Embedders that want the effect to actually work must call
/// `with_approval_sink` — an unconfigured sink silently no-op'ing as
/// "approved" would defeat the point of the effect.
pub struct NullApprovalSink;
impl ApprovalSink for NullApprovalSink {
    fn request(&self, _scope: &str, _reason: &str) -> Result<String, String> {
        Err("no ApprovalSink configured — call DefaultHandler::with_approval_sink".into())
    }
}

/// Interactive sink for `lex run`: prints the reason to stdout, blocks
/// on a stdin line. Empty input or a leading `n`/`N` denies; anything
/// else is the approved answer text.
pub struct StdinApprovalSink;
impl ApprovalSink for StdinApprovalSink {
    fn request(&self, scope: &str, reason: &str) -> Result<String, String> {
        use std::io::Write;
        print!("[approval:{scope}] {reason}  (blank/n to deny) > ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
        let answer = line.trim();
        if answer.is_empty() || answer.eq_ignore_ascii_case("n") || answer.eq_ignore_ascii_case("no") {
            Err("denied by operator".into())
        } else {
            Ok(answer.to_string())
        }
    }
}

impl DefaultHandler {
    /// `approval.request(scope, reason)` — scope allow-list mirrors
    /// `process.spawn`'s `--allow-proc` basename check above: empty
    /// `allow_approval` is a wildcard, non-empty requires an exact
    /// match. On a match, blocks on `self.approval_sink`.
    pub(super) fn dispatch_approval(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "request" => {
                let scope = expect_str(args.first())?.to_string();
                let reason = expect_str(args.get(1))?.to_string();
                if !self.policy.allow_approval.is_empty()
                    && !self.policy.allow_approval.iter().any(|a| a == &scope)
                {
                    return Ok(err(Value::Str(format!(
                        "approval.request: scope `{scope}` not in --allow-approval {:?}",
                        self.policy.allow_approval
                    ).into())));
                }
                match self.approval_sink.request(&scope, &reason) {
                    Ok(answer) => Ok(ok(Value::Str(answer.into()))),
                    Err(reason) => Ok(err(Value::Str(reason.into()))),
                }
            }
            other => Err(format!("unsupported approval.{other}")),
        }
    }
}
