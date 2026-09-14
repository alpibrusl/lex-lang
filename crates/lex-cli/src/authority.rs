//! `lex authority`: the least grant a program provably needs, and what
//! a change did to it.
//!
//! `lex check --allow-effects …` asks whether a program fits a policy
//! someone wrote. This asks the inverse — **what policy does this code
//! require?** — which is a question only an effect system can answer
//! soundly, and it is the question a reviewer actually has.
//!
//! ```text
//! lex authority derive src/                       # least grant, with the fn that needs each effect
//! lex authority diff --base old/ --head src/ --fail-on widening
//! ```
//!
//! The fold itself lives in [`lex_types::authority`] so that lex-os
//! derives the same grant from the same rows; this module adds the two
//! things only the authoring toolchain can: a path that may be a whole
//! package, and the *contributors* — which function is why each effect
//! is in the answer. A review that knows `net` appears is less useful
//! than one that knows `push_telemetry` is why.
//!
//! Nothing is derived from a program that does not type-check. The
//! whole derivation rests on the declared rows being a sound
//! over-approximation of the bodies, which is exactly what the checker
//! establishes — so a type error here is a refusal, not a warning.

use super::*;
use lex_types::authority::{
    derive_from_effects, diff as diff_authority, Authority, AuthorityDiff, MinimalityWitness,
    Verdict,
};
use lex_types::types::EffectKind;
use lex_types::EffectSet;
use std::collections::BTreeMap;
use std::path::PathBuf;

const USAGE: &str = "usage: lex authority <derive|diff> ...\n  \
     lex authority derive <file-or-dir>\n  \
     lex authority diff --base <file-or-dir> --head <file-or-dir> [--fail-on widening|any]";

pub(super) fn cmd_authority(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("derive") => cmd_derive(fmt, &args[1..]),
        Some("diff") => cmd_diff(fmt, &args[1..]),
        Some(other) => bail!("unknown `lex authority` subcommand: {other}\n{USAGE}"),
        None => bail!("{USAGE}"),
    }
}

/// A derivation plus the provenance the authoring toolchain can see.
struct Derived {
    authority: Authority,
    witness: Vec<MinimalityWitness>,
    /// effect kind → the functions declaring it, sorted.
    contributors: BTreeMap<String, Vec<String>>,
    files: usize,
}

fn derive_path(path: &str) -> Result<Derived> {
    let files = fmt::collect_lex_files(&[PathBuf::from(path)])?;
    if files.is_empty() {
        bail!("no .lex files under {path}");
    }
    let mut effects = EffectSet::empty();
    let mut contributors: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in &files {
        let display = file.display().to_string();
        let prog = read_program(&display)?;
        let stages = canonicalize_program(&prog);
        // A dishonest effect row makes every conclusion below unsound,
        // so the checker runs first and its failure is fatal.
        lex_types::check_program(&stages).map_err(|errs| {
            anyhow!(
                "{display} does not type-check ({} error(s)); \
                 authority cannot be derived from a program whose effect \
                 rows are unverified — run `lex check {display}`",
                errs.len()
            )
        })?;
        for stage in &stages {
            if let Stage::FnDecl(fd) = stage {
                for e in &fd.effects {
                    let kind = match &e.arg {
                        Some(lex_ast::EffectArg::Str { value }) => {
                            EffectKind::with_str(e.name.clone(), value.clone())
                        }
                        _ => EffectKind::bare(e.name.clone()),
                    };
                    effects.concrete.insert(kind);
                    let who = contributors.entry(e.name.clone()).or_default();
                    if !who.contains(&fd.name) {
                        who.push(fd.name.clone());
                    }
                }
            }
        }
    }
    for who in contributors.values_mut() {
        who.sort();
    }

    let authority = derive_from_effects(&effects)?;
    let witness = authority.minimality_witness(&effects);
    Ok(Derived {
        authority,
        witness,
        contributors,
        files: files.len(),
    })
}

fn cmd_derive(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut path: Option<&str> = None;
    for a in args {
        match a.as_str() {
            other if !other.starts_with("--") => {
                if path.is_some() {
                    bail!("{USAGE}");
                }
                path = Some(other);
            }
            other => bail!("unknown flag `{other}` for `lex authority derive`\n{USAGE}"),
        }
    }
    let path = path.ok_or_else(|| anyhow!("{USAGE}"))?;
    let d = derive_path(path)?;
    let a = &d.authority;

    let data = serde_json::json!({
        "path": path,
        "files": d.files,
        "authority": a,
        "authority_id": a.grant_id().0,
        "grant_pretty": a.grant.pretty(),
        "minimality_witness": d.witness,
        "contributors": d.contributors,
    });
    acli::emit_or_text("authority.derive", data, fmt, || {
        println!(
            "derived authority — {path} ({} file{})",
            d.files,
            if d.files == 1 { "" } else { "s" }
        );
        print_authority(a, &d.contributors);
        if d.witness.is_empty() {
            println!("  minimal      yes — this program needs no authority at all");
        } else {
            println!("  minimal      yes — tight on every dimension it uses:");
            for w in &d.witness {
                println!(
                    "                 {} at `{}`: lowering to `{}` rejects `{}`",
                    w.dimension, w.level, w.lowered_to, w.rejected_effect
                );
            }
        }
        // The run-time shape of the same answer, so an author can go
        // straight from "what does this need" to running it that way.
        println!("  run it with  lex run {} <file> <fn>", run_flags(a));
    });
    Ok(())
}

fn cmd_diff(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut base: Option<&str> = None;
    let mut head: Option<&str> = None;
    let mut fail_on: Option<&str> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => base = Some(it.next().ok_or_else(|| anyhow!("--base needs a path"))?),
            "--head" => head = Some(it.next().ok_or_else(|| anyhow!("--head needs a path"))?),
            "--fail-on" => {
                fail_on = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--fail-on needs a value"))?,
                )
            }
            other => bail!("unknown flag `{other}` for `lex authority diff`\n{USAGE}"),
        }
    }
    let (base, head) = match (base, head) {
        (Some(b), Some(h)) => (b, h),
        _ => bail!("{USAGE}"),
    };
    let refuse_at = match fail_on {
        None => None,
        Some("widening") => Some(Verdict::Widening),
        Some("any") => Some(Verdict::Narrowing),
        Some(other) => bail!("--fail-on takes `widening` or `any`, not `{other}`"),
    };

    let b = derive_path(base)?;
    let h = derive_path(head)?;
    let delta = diff_authority(&b.authority, &h.authority);
    let refuse = match refuse_at {
        Some(Verdict::Widening) => delta.verdict == Verdict::Widening,
        Some(_) => delta.verdict != Verdict::Unchanged,
        None => false,
    };

    let data = serde_json::json!({
        "base": base,
        "head": head,
        "base_authority": b.authority,
        "head_authority": h.authority,
        "delta": delta,
        "verdict": delta.verdict.as_str(),
        "refused": refuse,
    });
    acli::emit_or_text("authority.diff", data, fmt, || {
        println!("authority delta — {base} → {head}");
        print_diff(&delta, &h.contributors);
        println!("verdict: {}", verdict_line(delta.verdict));
    });
    if refuse {
        std::process::exit(::acli::ExitCode::PreconditionFailed.code());
    }
    Ok(())
}

// ------------------------------------------------------------ printing

fn print_authority(a: &Authority, contributors: &BTreeMap<String, Vec<String>>) {
    println!("  grant        {}", a.grant.pretty());
    println!("  authority-id {}", a.grant_id());
    if !a.egress.is_empty() {
        println!("  egress       {}", a.egress.join(", "));
    }
    if !a.fs_read.is_empty() {
        println!("  fs read      {}", a.fs_read.join(", "));
    }
    if !a.fs_write.is_empty() {
        println!("  fs write     {}", a.fs_write.join(", "));
    }
    for kind in &a.effects {
        let who = contributors
            .get(kind)
            .map(|v| v.join(", "))
            .unwrap_or_default();
        println!("  {kind:<12} {who}");
    }
    if !a.off_lattice.is_empty() {
        println!(
            "  off-lattice  {} (no grant refuses these — review them by eye)",
            a.off_lattice.join(", ")
        );
    }
    if a.unscoped_net {
        println!(
            "  net scope    partial — a bare [net] is present, so *which* host \
             is a perimeter question"
        );
    }
}

fn print_diff(d: &AuthorityDiff, contributors: &BTreeMap<String, Vec<String>>) {
    for dim in &d.dimensions {
        println!(
            "  {:<12} {} → {}{}",
            dim.dimension.to_string(),
            dim.from,
            dim.to,
            if dim.widens() {
                "   WIDENS"
            } else {
                "   narrows"
            }
        );
    }
    print_list("egress", "+", &d.egress_added);
    print_list("egress", "-", &d.egress_removed);
    print_list("fs read", "+", &d.fs_read_added);
    print_list("fs read", "-", &d.fs_read_removed);
    print_list("fs write", "+", &d.fs_write_added);
    print_list("fs write", "-", &d.fs_write_removed);
    for kind in &d.off_lattice_added {
        let who = contributors
            .get(kind)
            .map(|v| format!("  ({})", v.join(", ")))
            .unwrap_or_default();
        println!("  off-lattice  + {kind}{who}");
    }
    print_list("off-lattice", "-", &d.off_lattice_removed);
    if d.lost_net_precision {
        println!("  net scope    host-scoped → bare [net]   WIDENS (the static host is gone)");
    }
    if d.is_empty() {
        println!("  (nothing moved)");
    }
}

fn print_list(label: &str, sign: &str, items: &[String]) {
    for item in items {
        println!("  {label:<12} {sign} {item}");
    }
}

fn verdict_line(v: Verdict) -> &'static str {
    match v {
        Verdict::Unchanged => "UNCHANGED — this change reaches nowhere new",
        Verdict::Narrowing => {
            "NARROWING — the code proves it no longer needs some authority; safe to apply"
        }
        Verdict::Widening => {
            "WIDENING — this change reaches somewhere new; a human must approve it"
        }
    }
}

/// The `lex run` flags that grant exactly this authority and no more.
fn run_flags(a: &Authority) -> String {
    let mut parts = vec![format!("--allow-effects {}", a.effects.join(","))];
    for p in &a.fs_read {
        parts.push(format!("--allow-fs-read {p}"));
    }
    for p in &a.fs_write {
        parts.push(format!("--allow-fs-write {p}"));
    }
    for h in &a.egress {
        parts.push(format!("--allow-net-host {h}"));
    }
    parts.join(" ")
}
