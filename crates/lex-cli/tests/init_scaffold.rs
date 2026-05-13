//! `lex init` must drop a project that's `lex ci`-green from minute one.
//! The contract any agent reading the generated `AGENTS.md` relies on
//! is "run `lex ci` and trust the exit code" — if init shipped a red
//! baseline, that contract is broken before the agent writes a line.

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lex-init-scaffold-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_in(cwd: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn init_produces_all_expected_files() {
    let dir = unique_dir("files");
    let (code, _, stderr) = run_in(&dir, &["init", "my-app"]);
    assert_eq!(code, 0, "lex init exited {code}: {stderr}");

    for rel in [
        "lex.toml",
        "src/main.lex",
        "tests/test_main.lex",
        ".github/workflows/lex.yml",
        "AGENTS.md",
    ] {
        assert!(
            dir.join("my-app").join(rel).is_file(),
            "expected file not generated: {rel}"
        );
    }
}

#[test]
fn init_workflow_runs_lex_ci_after_explicit_steps() {
    // The generated workflow should keep the four named explicit steps
    // *and* end with a `lex ci` step, so failures stay categorised in
    // the GH Actions UI while the umbrella remains the source of truth.
    let dir = unique_dir("workflow");
    let (code, _, _) = run_in(&dir, &["init", "my-app"]);
    assert_eq!(code, 0);

    let yml = std::fs::read_to_string(dir.join("my-app/.github/workflows/lex.yml"))
        .expect("read workflow");
    for needle in [
        "lex pkg install",
        "lex check --strict src/main.lex",
        "lex fmt --check src/ tests/",
        "lex test",
        "lex ci",
    ] {
        assert!(yml.contains(needle), "workflow missing `{needle}`:\n{yml}");
    }
}

#[test]
fn init_agents_md_points_at_install_loop_and_upstream() {
    let dir = unique_dir("agents");
    let (code, _, _) = run_in(&dir, &["init", "my-app"]);
    assert_eq!(code, 0);

    let agents = std::fs::read_to_string(dir.join("my-app/AGENTS.md"))
        .expect("read AGENTS.md");

    // The three load-bearing sections an agent depends on.
    for marker in [
        // Title personalised to the project name.
        "AGENTS.md — my-app",
        // Install instructions (so an agent on a fresh box can bootstrap).
        "cargo build --release -p lex-cli",
        // The loop with `lex ci` as the gate.
        "lex ci",
        // Reference to the upstream cold-start guide.
        "docs/AGENT.md",
        // Effects-as-types reminder — the single biggest Lex-ism.
        "Effects are types",
    ] {
        assert!(agents.contains(marker), "AGENTS.md missing `{marker}`");
    }
}

#[test]
fn init_baseline_passes_lex_ci() {
    // The headline contract: a freshly-initialised project is
    // immediately green under `lex ci`. If this regresses, every
    // agent's first iteration starts in a broken state.
    let dir = unique_dir("ci-green");
    let (code, _, stderr) = run_in(&dir, &["init", "my-app"]);
    assert_eq!(code, 0, "lex init failed: {stderr}");

    let app = dir.join("my-app");
    let (code, stdout, stderr) = run_in(&app, &["ci"]);
    assert_eq!(
        code, 0,
        "lex ci must be green on a fresh `lex init`.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("CI passed"),
        "expected `CI passed` line in lex ci output, got:\n{stdout}"
    );
}
