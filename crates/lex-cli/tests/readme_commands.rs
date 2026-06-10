//! Keep the README honest about the CLI (lex-lang half of lex-os#22).
//!
//! The README presents a curated slice of a large CLI. Two ways it can rot:
//! it can invoke a command the binary no longer has (a rename/removal), or
//! it can promise an exit code the binary no longer returns. Both are
//! caught here, sourced from the code itself:
//!
//!  1. **Every README `lex <cmd>` is a real command.** The authoritative
//!     command set is the dispatch table in `src/main.rs` (`match
//!     cmd.as_str()`), parsed directly — no curated second list to drift.
//!     Every command the README invokes must be in it.
//!  2. **Documented exit codes hold.** The Quickstart's load-bearing exit
//!     codes are run for real against the built binary.
//!  3. **Every dispatched command is documented.** Both human help
//!     (`lex help`) and the agent surface (`lex skill`) must list every
//!     command the binary dispatches (bar a few self-describing meta
//!     commands) — so a new command can't ship undocumented.
//!  4. **The CLI surface is snapshotted.** `lex skill` is pinned to a
//!     committed golden file, so any change to a command's description,
//!     options, or exit codes fails CI until the snapshot is regenerated —
//!     the flag-level twin of guard 1, matching lex-os's
//!     `cli_reference_is_in_sync`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_lex");

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/lex-cli; the repo root is two up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The set of command tokens the binary dispatches, read straight from the
/// `match cmd.as_str() { … }` table in `src/main.rs`. A dispatch arm looks
/// like `"check" => …` or `"version" | "--version" | "-V" => …`; we accept
/// a line as an arm only when everything left of `=>` is string literals
/// joined by `|`, which excludes nested `match` arms in handler bodies
/// (e.g. `Ok(s) => s`).
fn dispatch_commands() -> BTreeSet<String> {
    let src =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"))
            .expect("read main.rs");

    let start = src
        .find("match cmd.as_str()")
        .expect("dispatch match not found");
    // The catch-all `other => bail!` ends the command arms.
    let end = src[start..]
        .find("other =>")
        .map(|o| start + o)
        .unwrap_or(src.len());
    let region = &src[start..end];

    let mut cmds = BTreeSet::new();
    for line in region.lines() {
        let Some(arrow) = line.find("=>") else {
            continue;
        };
        let lhs = &line[..arrow];
        let literals = quoted_literals(lhs);
        if literals.is_empty() {
            continue;
        }
        // Confirm the LHS is *only* those literals joined by `|` — i.e. a
        // pure dispatch key, not a code line that happens to contain `=>`.
        let mut residue = lhs.to_string();
        for lit in &literals {
            residue = residue.replacen(&format!("\"{lit}\""), "", 1);
        }
        if residue
            .trim()
            .chars()
            .all(|c| c == '|' || c.is_whitespace())
        {
            for lit in literals {
                cmds.insert(lit);
            }
        }
    }
    assert!(
        cmds.contains("check") && cmds.contains("run") && cmds.contains("pkg"),
        "dispatch parse looks wrong; got {cmds:?}"
    );
    cmds
}

/// Extract the contents of every double-quoted literal in `s`.
fn quoted_literals(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '"' {
            let mut lit = String::new();
            for (_, c2) in chars.by_ref() {
                if c2 == '"' {
                    break;
                }
                lit.push(c2);
            }
            out.push(lit);
        }
    }
    out
}

/// Every `lex <cmd>` invocation the README shows, reduced to its top-level
/// command token (the only level the dispatch table knows; `pkg publish`
/// dispatches on `pkg`). Excludes `lex-lang`, `lex.toml`, capitalised
/// "Lex", etc.
fn readme_commands() -> BTreeSet<String> {
    let text = std::fs::read_to_string(repo_root().join("README.md")).expect("read README");
    let mut cmds = BTreeSet::new();
    let bytes = text.as_bytes();
    let needle = "lex ";
    let mut from = 0;
    while let Some(rel) = text[from..].find(needle) {
        let i = from + rel;
        from = i + needle.len();
        // The char before `lex` must not be alphanumeric, `-`, or `.` —
        // which rules out `complex `, `lex-lang`, `flex `, and crucially the
        // `.lex ` *file extension* (`a_factorial.lex factorial` is not a
        // `lex factorial` command).
        if i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'.' {
                continue;
            }
        }
        // Next token after `lex `.
        let rest = &text[i + needle.len()..];
        let tok: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        // Allow the long-flag forms the dispatch also accepts.
        let tok = if tok.is_empty() && rest.starts_with("--") {
            rest.split_whitespace().next().unwrap_or("").to_string()
        } else {
            tok
        };
        if !tok.is_empty() {
            cmds.insert(tok);
        }
    }
    cmds
}

#[test]
fn every_readme_command_is_dispatched() {
    let dispatched = dispatch_commands();
    let documented = readme_commands();
    assert!(
        documented.len() >= 8,
        "expected to find many README commands, got {documented:?}"
    );
    let unknown: Vec<&String> = documented
        .iter()
        .filter(|c| !dispatched.contains(*c))
        .collect();
    assert!(
        unknown.is_empty(),
        "README invokes `lex <cmd>` for commands the binary doesn't dispatch: {unknown:?}\n\
         dispatched: {dispatched:?}"
    );
}

fn run(args: &[&str]) -> i32 {
    Command::new(BIN)
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("spawn lex")
        .status
        .code()
        .unwrap_or(-1)
}

#[test]
fn documented_exit_codes_hold() {
    // Quickstart: a pure example type-checks (exit 0).
    assert_eq!(
        run(&["check", "examples/a_factorial.lex"]),
        0,
        "`lex check examples/a_factorial.lex` should succeed"
    );

    // Quickstart: an LLM-emitted body that reads the filesystem under a
    // `net`-only grant is rejected before it runs — the README promises
    // exit 2 for this exact case.
    assert_eq!(
        run(&[
            "agent-tool",
            "--allow-effects",
            "net",
            "--input",
            "url",
            "--body",
            "match io.read(\"/etc/passwd\") { Ok(s) => s, Err(e) => e }",
        ]),
        2,
        "an effect-lying agent body should be TYPE-CHECK REJECTED with exit 2"
    );
}

/// Self-describing meta commands that needn't appear in the command listings
/// (`version`/`help` are output modes; `introspect`/`skill` *are* the surfaces).
const META: &[&str] = &["help", "introspect", "skill", "version"];

/// Capture stdout of `lex <args>`, with the crate version normalized to
/// `<VERSION>` so the skill snapshot survives releases (the only volatile token
/// in `lex skill` is the `vX.Y.Z` in its header).
fn run_stdout(args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("spawn lex");
    String::from_utf8_lossy(&out.stdout).replace(env!("CARGO_PKG_VERSION"), "<VERSION>")
}

/// Top-level command tokens listed by `lex help` (`print_usage`): the first
/// word of each two-space-indented command row (continuation lines and the
/// `--flag` rows start otherwise and are skipped).
fn help_commands(out: &str) -> BTreeSet<String> {
    let mut cmds = BTreeSet::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_lowercase()) {
            continue;
        }
        let tok: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !tok.is_empty() {
            cmds.insert(tok);
        }
    }
    cmds
}

/// Command tokens advertised by `lex skill`: the word after ``- `lex `` in
/// each bullet of the "Available commands" list.
fn skill_commands(out: &str) -> BTreeSet<String> {
    let mut cmds = BTreeSet::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("- `lex ") else {
            continue;
        };
        let tok: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !tok.is_empty() {
            cmds.insert(tok);
        }
    }
    cmds
}

#[test]
fn every_dispatched_command_is_documented() {
    let dispatched: BTreeSet<String> = dispatch_commands()
        .into_iter()
        // Drop flag-style aliases (`--help`, `-V`, …) and self-describing meta.
        .filter(|c| !c.starts_with('-') && !META.contains(&c.as_str()))
        .collect();
    let help = help_commands(&run_stdout(&["help"]));
    let skill = skill_commands(&run_stdout(&["skill"]));

    let missing_help: Vec<&String> = dispatched.iter().filter(|c| !help.contains(*c)).collect();
    let missing_skill: Vec<&String> = dispatched.iter().filter(|c| !skill.contains(*c)).collect();
    assert!(
        missing_help.is_empty(),
        "`lex help` omits dispatched commands: {missing_help:?} — add them to print_usage()"
    );
    assert!(
        missing_skill.is_empty(),
        "`lex skill` omits dispatched commands: {missing_skill:?} — add a CommandInfo in acli.rs::commands()"
    );
}

#[test]
fn cli_skill_is_in_sync() {
    let current = run_stdout(&["skill"]);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cli-skill.txt");
    if std::env::var_os("UPDATE_CLI_SKILL").is_some() {
        std::fs::write(&path, &current).expect("write skill snapshot");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        current, committed,
        "`lex skill` surface drifted from tests/cli-skill.txt.\n\
         Regenerate: UPDATE_CLI_SKILL=1 cargo test -p lex-cli --test readme_commands cli_skill_is_in_sync"
    );
}
