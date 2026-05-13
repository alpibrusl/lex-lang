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
fn init_workflow_downloads_pinned_binary_not_cargo_build() {
    // Workflow should install lex by downloading the pre-built binary
    // from the GitHub Release pinned to the toolchain version that
    // scaffolded the project — not by cloning + `cargo build`. The
    // binary path is ~30s; building was ~3min. Pinning keeps CI
    // reproducible across toolchain bumps.
    let dir = unique_dir("workflow-binary");
    let (code, _, _) = run_in(&dir, &["init", "my-app"]);
    assert_eq!(code, 0);

    let yml = std::fs::read_to_string(dir.join("my-app/.github/workflows/lex.yml"))
        .expect("read workflow");

    let expected_version = env!("CARGO_PKG_VERSION");

    assert!(
        yml.contains(&format!("LEX_VERSION: v{expected_version}")),
        "workflow should pin LEX_VERSION to the scaffolding toolchain version (v{expected_version}):\n{yml}"
    );
    assert!(
        yml.contains("github.com/alpibrusl/lex-lang/releases/download"),
        "workflow should download from GitHub Releases:\n{yml}"
    );
    assert!(
        !yml.contains("cargo build"),
        "workflow should NOT cargo-build the toolchain (slow + needs Rust):\n{yml}"
    );
    assert!(
        !yml.contains("git clone"),
        "workflow should NOT clone lex-lang at CI time:\n{yml}"
    );
}

#[test]
fn init_agents_md_points_at_install_loop_and_upstream() {
    let dir = unique_dir("agents");
    let (code, _, _) = run_in(&dir, &["init", "my-app"]);
    assert_eq!(code, 0);

    let agents = std::fs::read_to_string(dir.join("my-app/AGENTS.md"))
        .expect("read AGENTS.md");

    // The load-bearing sections an agent depends on.
    let expected_version = env!("CARGO_PKG_VERSION");
    for marker in &[
        // Title personalised to the project name.
        "AGENTS.md — my-app".to_string(),
        // Primary install path: pre-built binary download.
        "github.com/alpibrusl/lex-lang/releases/download".to_string(),
        // Pinned to the toolchain version that scaffolded the project.
        format!("v{expected_version}"),
        // Cross-platform target detection so the recipe works on
        // Linux x86_64 / aarch64 and macOS Intel / Apple Silicon.
        "uname -s".to_string(),
        // Fallback `cargo build` still documented for off-release versions.
        "cargo build --release -p lex-cli".to_string(),
        // The loop with `lex ci` as the gate.
        "lex ci".to_string(),
        // Reference to the upstream cold-start guide.
        "docs/AGENT.md".to_string(),
        // Effects-as-types reminder — the single biggest Lex-ism.
        "Effects are types".to_string(),
    ] {
        assert!(
            agents.contains(marker.as_str()),
            "AGENTS.md missing `{marker}`"
        );
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
