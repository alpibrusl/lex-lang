//! `lex doc-sync` — generated documentation blocks with a `--check` twin.
//!
//! The drift problem this closes: reference material that lives in docs but
//! whose source of truth is code (an effect list, a builtin table, an event
//! vocabulary, a CLI surface) rots silently, because nothing fails when the
//! code moves and the doc doesn't. The fix, borrowed from the
//! deepseek-harness review: every such doc region is *generated* by a
//! command, and CI re-runs the generator and diffs — drift is a red build
//! with the one-command fix in the message, never a discovery.
//!
//! One mechanism for the whole ecosystem: the manifest's generator is an
//! arbitrary shell command, so a Rust repo generates with `cargo run`, a
//! Lex repo with `lex run`, a TypeScript repo with `node`. Every repo
//! already installs the pinned `lex` toolchain in CI, so shipping the
//! runner in the CLI gives all of them the same two entry points:
//!
//! ```sh
//! lex doc-sync            # regenerate every target in docsync.toml
//! lex doc-sync --check    # CI: fail on drift, naming each stale target
//! ```
//!
//! `docsync.toml`, at the repo root (or passed explicitly):
//!
//! ```toml
//! # A whole file owned by a generator (e.g. downstream AGENTS.md copies):
//! [[file]]
//! path = "AGENTS.md"
//! command = "lex agent-guidelines"
//!
//! # A marked region inside a hand-written doc:
//! [[block]]
//! id = "effects"
//! path = "docs/AGENT.md"
//! command = "cargo run -q -p lex-cli -- docs --effects"
//! ```
//!
//! Block targets replace the lines between `<!-- docsync:begin ID -->` and
//! `<!-- docsync:end ID -->` (matched by substring, so any comment syntax
//! that can carry those tokens works). The markers themselves stay, so
//! regeneration is idempotent and the surrounding prose remains
//! hand-written. Generator stdout is the block body; a trailing newline is
//! normalized. A failing generator, a missing marker pair, or a duplicate
//! id is an error naming the target — refuse, don't guess.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    file: Vec<FileTarget>,
    #[serde(default)]
    block: Vec<BlockTarget>,
}

#[derive(Debug, Deserialize)]
struct FileTarget {
    path: String,
    command: String,
}

#[derive(Debug, Deserialize)]
struct BlockTarget {
    id: String,
    path: String,
    command: String,
}

pub fn cmd_doc_sync(args: &[String]) -> Result<()> {
    let mut check = false;
    let mut manifest_path: Option<PathBuf> = None;
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            "--help" | "-h" => {
                println!(
                    "usage: lex doc-sync [--check] [manifest]\n\
                     \n\
                     Regenerate (or, with --check, verify) every generated doc target\n\
                     listed in docsync.toml. See the module docs for the manifest format."
                );
                return Ok(());
            }
            other if !other.starts_with('-') => manifest_path = Some(PathBuf::from(other)),
            other => bail!("unknown flag `{other}`; usage: lex doc-sync [--check] [manifest]"),
        }
    }
    let manifest_path = manifest_path.unwrap_or_else(|| PathBuf::from("docsync.toml"));
    let root = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("cannot read manifest {}", manifest_path.display()))?;
    let manifest: Manifest = toml::from_str(&raw)
        .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
    if manifest.file.is_empty() && manifest.block.is_empty() {
        bail!(
            "{} declares no [[file]] or [[block]] targets",
            manifest_path.display()
        );
    }
    {
        let mut seen = std::collections::BTreeSet::new();
        for b in &manifest.block {
            if !seen.insert((&b.path, &b.id)) {
                bail!("duplicate block id `{}` for {}", b.id, b.path);
            }
        }
    }

    let mut drifted: Vec<String> = Vec::new();
    for t in &manifest.file {
        let target = root.join(&t.path);
        let generated = normalize_trailing(run_generator(&root, &t.command, &t.path)?);
        let current = fs::read_to_string(&target).unwrap_or_default();
        apply_or_report(check, &target, &t.path, &current, &generated, &mut drifted)?;
    }
    for b in &manifest.block {
        let target = root.join(&b.path);
        let current = fs::read_to_string(&target)
            .with_context(|| format!("cannot read block target {}", target.display()))?;
        let body = run_generator(&root, &b.command, &b.path)?;
        let updated = splice_block(&current, &b.id, &body)
            .with_context(|| format!("in {}", target.display()))?;
        let label = format!("{}#{}", b.path, b.id);
        apply_or_report(check, &target, &label, &current, &updated, &mut drifted)?;
    }

    if check {
        if drifted.is_empty() {
            println!("doc-sync: all targets current");
            Ok(())
        } else {
            bail!(
                "doc-sync: {} target(s) drifted from their generators: {}\n  fix: lex doc-sync",
                drifted.len(),
                drifted.join(", ")
            )
        }
    } else {
        if drifted.is_empty() {
            println!("doc-sync: all targets already current");
        }
        Ok(())
    }
}

fn apply_or_report(
    check: bool,
    target: &Path,
    label: &str,
    current: &str,
    updated: &str,
    drifted: &mut Vec<String>,
) -> Result<()> {
    if current == updated {
        return Ok(());
    }
    if check {
        drifted.push(label.to_string());
    } else {
        fs::write(target, updated).with_context(|| format!("cannot write {}", target.display()))?;
        println!("doc-sync: regenerated {label}");
        drifted.push(label.to_string());
    }
    Ok(())
}

/// Run one generator through the shell with the manifest's directory as
/// cwd. Stdout is the product; a non-zero exit is an error naming the
/// target, with the generator's stderr attached.
fn run_generator(root: &Path, command: &str, label: &str) -> Result<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .output()
        .with_context(|| format!("generator for {label} failed to spawn: `{command}`"))?;
    if !out.status.success() {
        bail!(
            "generator for {label} exited {:?}: `{command}`\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8(out.stdout)
        .map_err(|_| anyhow!("generator for {label} produced non-UTF-8 output"))
}

fn normalize_trailing(mut s: String) -> String {
    while s.ends_with('\n') {
        s.pop();
    }
    s.push('\n');
    s
}

/// Replace the lines strictly between the begin/end markers for `id`,
/// keeping the marker lines themselves. Exactly one begin and one end, in
/// order — anything else is an error, not a guess.
fn splice_block(doc: &str, id: &str, body: &str) -> Result<String> {
    let begin_tok = format!("docsync:begin {id}");
    let end_tok = format!("docsync:end {id}");
    let lines: Vec<&str> = doc.split_inclusive('\n').collect();
    let idx = |tok: &str| -> Result<usize> {
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(tok))
            .map(|(i, _)| i)
            .collect();
        match hits.as_slice() {
            [i] => Ok(*i),
            [] => bail!("marker `{tok}` not found"),
            _ => bail!(
                "marker `{tok}` appears {} times; must be unique",
                hits.len()
            ),
        }
    };
    let b = idx(&begin_tok)?;
    let e = idx(&end_tok)?;
    if e <= b {
        bail!("marker `{end_tok}` appears before `{begin_tok}`");
    }
    let mut out = String::new();
    for l in &lines[..=b] {
        out.push_str(l);
    }
    let mut body = body.trim_end_matches('\n').to_string();
    if !body.is_empty() {
        body.push('\n');
    }
    out.push_str(&body);
    for l in &lines[e..] {
        out.push_str(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_replaces_only_the_marked_region() {
        let doc = "intro\n<!-- docsync:begin x -->\nOLD\n<!-- docsync:end x -->\noutro\n";
        let got = splice_block(doc, "x", "NEW LINE 1\nNEW LINE 2\n").unwrap();
        assert_eq!(
            got,
            "intro\n<!-- docsync:begin x -->\nNEW LINE 1\nNEW LINE 2\n<!-- docsync:end x -->\noutro\n"
        );
        // Idempotent: splicing the same body again changes nothing.
        assert_eq!(
            splice_block(&got, "x", "NEW LINE 1\nNEW LINE 2\n").unwrap(),
            got
        );
    }

    #[test]
    fn splice_works_with_non_html_comment_syntax() {
        let doc = "# docsync:begin cfg\nold\n# docsync:end cfg\n";
        let got = splice_block(doc, "cfg", "new\n").unwrap();
        assert_eq!(got, "# docsync:begin cfg\nnew\n# docsync:end cfg\n");
    }

    #[test]
    fn splice_refuses_missing_or_duplicate_markers() {
        assert!(splice_block("no markers\n", "x", "b").is_err());
        let dup = "<!-- docsync:begin x -->\n<!-- docsync:end x -->\n<!-- docsync:begin x -->\n";
        assert!(splice_block(dup, "x", "b").is_err());
        let reversed = "<!-- docsync:end x -->\n<!-- docsync:begin x -->\n";
        assert!(splice_block(reversed, "x", "b").is_err());
    }

    #[test]
    fn end_to_end_regenerate_and_check() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("doc.md");
        fs::write(
            &doc,
            "head\n<!-- docsync:begin gen -->\nstale\n<!-- docsync:end gen -->\n",
        )
        .unwrap();
        let whole = dir.path().join("WHOLE.md");
        fs::write(&whole, "stale whole\n").unwrap();
        let manifest = dir.path().join("docsync.toml");
        fs::write(
            &manifest,
            "[[file]]\npath = \"WHOLE.md\"\ncommand = \"printf 'fresh whole\\n'\"\n\n\
             [[block]]\nid = \"gen\"\npath = \"doc.md\"\ncommand = \"printf 'fresh\\n'\"\n",
        )
        .unwrap();
        let mpath = manifest.to_string_lossy().to_string();

        // --check on stale targets fails and names both.
        let err = cmd_doc_sync(&["--check".into(), mpath.clone()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("WHOLE.md"), "{err}");
        assert!(err.contains("doc.md#gen"), "{err}");
        assert!(err.contains("fix: lex doc-sync"), "{err}");

        // Regenerate, then --check passes and content is right.
        cmd_doc_sync(std::slice::from_ref(&mpath)).unwrap();
        assert_eq!(fs::read_to_string(&whole).unwrap(), "fresh whole\n");
        assert_eq!(
            fs::read_to_string(&doc).unwrap(),
            "head\n<!-- docsync:begin gen -->\nfresh\n<!-- docsync:end gen -->\n"
        );
        cmd_doc_sync(&["--check".into(), mpath]).unwrap();
    }

    #[test]
    fn failing_generator_is_an_error_naming_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("docsync.toml");
        fs::write(
            &manifest,
            "[[file]]\npath = \"X.md\"\ncommand = \"echo boom >&2; exit 3\"\n",
        )
        .unwrap();
        let err = cmd_doc_sync(&[manifest.to_string_lossy().to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("X.md"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }
}
