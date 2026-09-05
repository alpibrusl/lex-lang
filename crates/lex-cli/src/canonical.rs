//! `lex canonical encode|decode` and `lex hash`: canonical-AST wire format and content hashes.

use super::*;

/// `lex canonical <encode|decode>` dispatcher (#206 slice 2).
pub(super) fn cmd_canonical(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex canonical <encode|decode> ..."))?;
    match sub.as_str() {
        "encode" => cmd_canonical_encode(fmt, &args[1..]),
        "decode" => cmd_canonical_decode(fmt, &args[1..]),
        other => bail!(
            "unknown `lex canonical` action `{other}`; \
                       expected `encode` or `decode`"
        ),
    }
}

/// `lex canonical encode <text-file> [--out <bytes-file>]` (#206 slice 2).
///
/// Parses a `.lex` source file, canonicalizes it, and emits the
/// versioned canonical-AST byte representation. Without `--out`,
/// writes raw bytes to stdout (suitable for piping into another
/// agent process or `lex canonical decode`); with `--out`, writes
/// to the named file.
///
/// JSON-output mode (`--output json`) emits a structured envelope
/// instead — `{ "ok": true, "bytes_b64": "..." }` — so agent
/// harnesses can capture the canonical bytes without dealing with
/// raw-bytes-on-stdout encoding issues.
pub(super) fn cmd_canonical_encode(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut path: Option<&str> = None;
    let mut out_path: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out_path = Some(
                    args.get(i + 1)
                        .map(|s| s.as_str())
                        .ok_or_else(|| anyhow!("--out needs a path"))?,
                );
                i += 2;
            }
            s if s.starts_with("--") => bail!("unknown flag `{s}` for `lex canonical encode`"),
            _ => {
                if path.is_some() {
                    bail!("usage: lex canonical encode <text-file> [--out <bytes-file>]");
                }
                path = Some(args[i].as_str());
                i += 1;
            }
        }
    }
    let path = path
        .ok_or_else(|| anyhow!("usage: lex canonical encode <text-file> [--out <bytes-file>]"))?;

    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let bytes = lex_ast::canonical_format::encode_program(&stages);

    if let Some(out) = out_path {
        std::fs::write(out, &bytes).map_err(|e| anyhow!("write {out}: {e}"))?;
        let data = serde_json::json!({
            "ok": true,
            "out": out,
            "bytes": bytes.len(),
            "stages": stages.len(),
        });
        acli::emit_or_text("canonical-encode", data, fmt, || {
            println!("wrote {} bytes to {out}", bytes.len());
        });
    } else {
        match fmt {
            OutputFormat::Json => {
                let b64 = encode_b64(&bytes);
                let data = serde_json::json!({
                    "ok": true,
                    "bytes_b64": b64,
                    "stages": stages.len(),
                });
                acli::emit_or_text("canonical-encode", data, fmt, || {});
            }
            _ => {
                use std::io::Write;
                std::io::stdout()
                    .write_all(&bytes)
                    .map_err(|e| anyhow!("stdout: {e}"))?;
            }
        }
    }
    Ok(())
}

pub(super) fn cmd_canonical_decode(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let path = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex canonical decode <bytes-file>"))?;
    let bytes = std::fs::read(path).map_err(|e| anyhow!("read {path}: {e}"))?;
    let stages = lex_ast::canonical_format::decode_program(&bytes)
        .map_err(|e| anyhow!("decode {path}: {e}"))?;
    let text = lex_ast::print_stages(&stages);
    let data = serde_json::json!({
        "ok": true,
        "stages": stages.len(),
        "text": &text,
    });
    acli::emit_or_text("canonical-decode", data, fmt, || {
        print!("{text}");
    });
    Ok(())
}

/// Tiny base64 encoder for the JSON envelope output. Avoids adding
/// a `base64` crate dep just for this CLI surface — standard
/// alphabet (RFC 4648 §4), padded.
pub(super) fn encode_b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i] as usize;
        let b1 = bytes[i + 1] as usize;
        let b2 = bytes[i + 2] as usize;
        out.push(A[b0 >> 2] as char);
        out.push(A[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
        out.push(A[((b1 & 0b1111) << 2) | (b2 >> 6)] as char);
        out.push(A[b2 & 0b111111] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let b0 = bytes[i] as usize;
        out.push(A[b0 >> 2] as char);
        out.push(A[(b0 & 0b11) << 4] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let b0 = bytes[i] as usize;
        let b1 = bytes[i + 1] as usize;
        out.push(A[b0 >> 2] as char);
        out.push(A[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
        out.push(A[(b1 & 0b1111) << 2] as char);
        out.push('=');
    }
    out
}

pub(super) fn cmd_hash(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let path = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex hash <file>"))?;
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let entries: Vec<serde_json::Value> = stages
        .iter()
        .map(|s| {
            let name = stage_name(s);
            let h = stage_canonical_hash_hex(s);
            let sid = stage_id(s).unwrap_or_else(|| "-".into());
            serde_json::json!({
                "name": name,
                "canonical_ast": h,
                "stage_id": sid,
            })
        })
        .collect();
    let data = serde_json::json!({ "stages": entries });
    acli::emit_or_text("hash", data, fmt, || {
        for s in &stages {
            let name = stage_name(s);
            let h = stage_canonical_hash_hex(s);
            let sid = stage_id(s).unwrap_or_else(|| "-".into());
            println!("{name}\tcanonical_ast={h}\tstage_id={sid}");
        }
    });
    Ok(())
}
