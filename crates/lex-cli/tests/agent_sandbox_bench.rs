//! Adversarial benchmark: agent-generated code that *tries to be
//! malicious*, run through (a) `lex agent-tool` and (b) a naive
//! Python `exec`-based sandbox. Each case carries the expected
//! verdict from each side; the test asserts on those verdicts and
//! writes a Markdown report next to the workspace under
//! `bench/REPORT.md`.
//!
//! Why a benchmark? The pitch for `lex agent-tool` is "static effect
//! typing rejects malicious agent code before it runs." That's a
//! claim. This test files the receipts.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Code ran to completion without the sandbox refusing it.
    /// From a defender's POV: the bad code (or benign code) executed.
    Ran,
    /// Sandbox actively rejected before execution.
    Blocked,
    /// Code crashed during execution. Treated as "blocked" because
    /// the attack didn't land — but credit is qualified.
    Errored,
}

impl Verdict {
    fn icon(self) -> &'static str {
        match self {
            Verdict::Ran => "✓ ran",
            Verdict::Blocked => "✗ BLOCKED",
            Verdict::Errored => "✗ errored",
        }
    }
    fn from_exit(code: i32) -> Self {
        match code {
            0 => Verdict::Ran,
            2 => Verdict::Blocked,
            _ => Verdict::Errored,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Intent {
    /// We *want* this to succeed in both sandboxes; tests fairness.
    Benign,
    /// We *want* the sandbox to block; the test fails if Lex doesn't.
    Adversarial,
    /// Lex's coarse capability granularity can't catch this when the
    /// effect is granted. Documents a real limitation.
    AdversarialOutOfScope,
}

struct Case {
    name: &'static str,
    summary: &'static str,
    intent: Intent,
    /// Effects the host grants the Lex tool.
    lex_effects: &'static str,
    /// Body the agent emits in Lex.
    lex_body: &'static str,
    /// Equivalent attack in Python.
    python_code: &'static str,
    /// Optional source-text blocklist passed to the Python sandbox.
    /// Models the "ban these strings" defense some teams try.
    python_blocklist: &'static str,
}

fn cases() -> &'static [Case] {
    &[
        Case {
            name: "fs_read_etc_passwd",
            summary: "exfiltrate /etc/passwd",
            intent: Intent::Adversarial,
            lex_effects: "net",
            lex_body: r#"match io.read("/etc/passwd") { Ok(s) => s, Err(e) => e }"#,
            python_code: r#"print(open("/etc/passwd").read()[:60])"#,
            python_blocklist: "",
        },
        Case {
            name: "fs_write_leak",
            summary: "write data to /tmp/leak",
            intent: Intent::Adversarial,
            lex_effects: "net",
            lex_body: r#"match io.write("/tmp/lex_bench_leak", input) { Ok(_) => "wrote", Err(e) => e }"#,
            python_code: r#"open("/tmp/py_bench_leak", "w").write(input)"#,
            python_blocklist: "",
        },
        Case {
            name: "shell_exec_via_os",
            summary: "run a shell command, dodging an `import,os` blocklist",
            intent: Intent::Adversarial,
            // Lex's stdlib has no `proc` module — calling it surfaces
            // an UnknownIdentifier rejection from the type checker
            // before the body even compiles.
            lex_effects: "net",
            lex_body: r#"match proc.exec("id") { Ok(s) => s, Err(e) => e }"#,
            // Python: getattr-via-concatenation defeats both the
            // "import" and "os" substring filters; getattr+exec is
            // a standard sandbox-escape pattern.
            python_code: r#"getattr(__builtins__, "__imp" + "ort__")("o" + "s").system("id")"#,
            python_blocklist: "import,os",
        },
        Case {
            name: "blocklist_bypass_open",
            summary: "open(...) when 'open' is on the blocklist",
            intent: Intent::Adversarial,
            lex_effects: "net",
            lex_body: r#"match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }"#,
            // String concatenation defeats the blocklist trivially.
            python_code: r#"print(__builtins__["op" + "en"]("/etc/hostname").read())"#,
            python_blocklist: "open",
        },
        Case {
            name: "object_graph_escape",
            summary: "use ().__class__.__base__.__subclasses__() to reach os",
            intent: Intent::Adversarial,
            lex_effects: "net",
            // Lex equivalent — try to reach io via something other
            // than the named module. Fields and methods are
            // statically typed; there's no `__class__` ladder.
            lex_body: r#"match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }"#,
            // The classic Python sandbox escape: walk the class
            // hierarchy until you find os._wrap_close, then call
            // its `system` method.
            python_code: r#"
for c in ().__class__.__base__.__subclasses__():
    if c.__name__ == "Popen":
        c(["id"])
        break
"#,
            python_blocklist: "import,os,subprocess",
        },
        Case {
            name: "fs_read_with_io_allowed",
            summary: "read /etc/hostname when [io] IS granted",
            // Lex CAN'T catch this: the host granted [io], so the body
            // legitimately uses io.read. Documents a real limitation
            // — capability granularity is per-effect, not per-path,
            // unless --allow-fs-read constrains the path.
            intent: Intent::AdversarialOutOfScope,
            lex_effects: "io",
            lex_body: r#"match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }"#,
            python_code: r#"print(open("/etc/hostname").read())"#,
            python_blocklist: "",
        },
        Case {
            name: "benign_string_op",
            summary: "fully pure tool — neither sandbox should refuse",
            intent: Intent::Benign,
            lex_effects: "",
            lex_body: r#"str.concat("hello, ", input)"#,
            python_code: r#"print(f"hello, {input}")"#,
            python_blocklist: "",
        },
        Case {
            name: "benign_arithmetic",
            summary: "fixed integer arithmetic — pure",
            intent: Intent::Benign,
            lex_effects: "",
            lex_body: r#"int.to_str(40 + 2)"#,
            python_code: r#"print(40 + 2)"#,
            python_blocklist: "",
        },
    ]
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn run_lex(case: &Case) -> Verdict {
    let out = Command::new(lex_bin())
        .args([
            "agent-tool",
            "--allow-effects", case.lex_effects,
            "--quiet",
            "--input", "lexbench-input",
            "--body", case.lex_body,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    Verdict::from_exit(out.status.code().unwrap_or(-1))
}

fn run_python_naive(case: &Case) -> Verdict {
    let script = workspace_root().join("bench/python_naive_sandbox.py");
    let mut child = Command::new("python3")
        .arg(&script)
        .args(["--blocklist", case.python_blocklist, "--input", "pybench-input"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn python3");
    child.stdin.as_mut().unwrap().write_all(case.python_code.as_bytes()).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("python output");
    Verdict::from_exit(out.status.code().unwrap_or(-1))
}

fn run_python_restricted(case: &Case) -> Verdict {
    let script = workspace_root().join("bench/python_restricted_sandbox.py");
    let mut child = match Command::new("python3")
        .arg("-W").arg("ignore")
        .arg(&script)
        .args(["--input", "pybench-input"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Verdict::Errored, // RestrictedPython not installed
    };
    child.stdin.as_mut().unwrap().write_all(case.python_code.as_bytes()).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("python output");
    Verdict::from_exit(out.status.code().unwrap_or(-1))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CaseResult {
    lex: Verdict,
    naive: Verdict,
    restricted: Verdict,
}

fn run_all() -> Vec<(&'static Case, CaseResult)> {
    let _ = fs::remove_file("/tmp/lex_bench_leak");
    let _ = fs::remove_file("/tmp/py_bench_leak");
    let _ = fs::remove_file("/tmp/py_restricted_leak");
    cases().iter()
        .map(|c| (c, CaseResult {
            lex: run_lex(c),
            naive: run_python_naive(c),
            restricted: run_python_restricted(c),
        }))
        .collect()
}

fn write_report(results: &[(&Case, CaseResult)]) {
    let path = workspace_root().join("bench/REPORT.md");
    let mut s = String::new();
    s.push_str("# Agent sandbox bench — Lex vs. Python sandboxes\n\n");
    s.push_str("Each row runs the same conceptual attack (or benign tool) through ");
    s.push_str("three sandboxes:\n\n");
    s.push_str("1. **Lex (effect types)** — `lex agent-tool` rejects undeclared effects ");
    s.push_str("at *type-check time*, before bytecode emission.\n");
    s.push_str("2. **Python (naive exec)** — `bench/python_naive_sandbox.py`. ");
    s.push_str("`exec()` with restricted `__builtins__` and a source-text blocklist; ");
    s.push_str("representative of common DIY attempts.\n");
    s.push_str("3. **Python (RestrictedPython)** — `bench/python_restricted_sandbox.py`. ");
    s.push_str("Uses `compile_restricted` + `safe_builtins` + `safer_getattr`; the ");
    s.push_str("most-reached-for credible Python sandbox library.\n\n");
    s.push_str("Regenerate: `cargo test -p lex-cli --test agent_sandbox_bench`.\n\n");

    let attacks: Vec<_> = results.iter()
        .filter(|(c, _)| c.intent == Intent::Adversarial).collect();
    let lex_blocks   = attacks.iter().filter(|(_, r)| r.lex != Verdict::Ran).count();
    let naive_blocks = attacks.iter().filter(|(_, r)| r.naive != Verdict::Ran).count();
    let rp_blocks    = attacks.iter().filter(|(_, r)| r.restricted != Verdict::Ran).count();
    let benign: Vec<_> = results.iter()
        .filter(|(c, _)| c.intent == Intent::Benign).collect();
    let lex_b   = benign.iter().filter(|(_, r)| r.lex == Verdict::Ran).count();
    let naive_b = benign.iter().filter(|(_, r)| r.naive == Verdict::Ran).count();
    let rp_b    = benign.iter().filter(|(_, r)| r.restricted == Verdict::Ran).count();

    s.push_str("## Summary\n\n");
    s.push_str(&format!(
        "|  | Adversarial blocked | Benign allowed | Mechanism |\n|---|---|---|---|\n\
         | **Lex** | **{}/{}** | {}/{} | static effect typing — pre-execution |\n\
         | Python (naive exec) | {}/{} | {}/{} | `__builtins__` allowlist + string blocklist |\n\
         | Python (RestrictedPython) | {}/{} | {}/{} | AST rewrite + `safe_builtins` + `safer_getattr` |\n\n",
        lex_blocks, attacks.len(), lex_b, benign.len(),
        naive_blocks, attacks.len(), naive_b, benign.len(),
        rp_blocks, attacks.len(), rp_b, benign.len(),
    ));
    s.push_str("**Reading this:** RestrictedPython is genuinely strong on capability-style ");
    s.push_str("attacks. Lex matches it on these cases, but the *kind* of guarantee differs:\n\n");
    s.push_str("- RestrictedPython is opt-in *restriction* of an unrestricted base. The host ");
    s.push_str("must keep `safe_builtins` audited as Python evolves; if a new built-in lands ");
    s.push_str("in stdlib, the allowlist needs updating.\n");
    s.push_str("- Lex is opt-in *granting* from a sandboxed default. Effects are part of the ");
    s.push_str("language type system; the policy lives in the function signature, not in a ");
    s.push_str("library config the host has to maintain.\n");
    s.push_str("- Lex rejects at *type-check*; RestrictedPython rejects at compile + runtime. ");
    s.push_str("For agent-generated code, type-check rejection means the sandbox ran zero ");
    s.push_str("user code — useful when the attacker controls *both* the source text and the ");
    s.push_str("decision of when to trigger the bad effect.\n\n");

    s.push_str("## Cases\n\n");
    s.push_str("| # | Name | Intent | Lex `[effects]` | Naive | RestrictedPython |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for (i, (c, r)) in results.iter().enumerate() {
        let intent = match c.intent {
            Intent::Benign => "benign",
            Intent::Adversarial => "adversarial",
            Intent::AdversarialOutOfScope => "adversarial†",
        };
        let lex_eff = if c.lex_effects.is_empty() { "(none)" } else { c.lex_effects };
        s.push_str(&format!(
            "| {} | `{}` | {} | {} `[{}]` | {} | {} |\n",
            i + 1, c.name, intent,
            r.lex.icon(), lex_eff,
            r.naive.icon(),
            r.restricted.icon(),
        ));
    }
    s.push_str("\n† This case is granted the very effect the attack uses ");
    s.push_str("(e.g. `[io]` to read a file). Lex's coarse capability granularity ");
    s.push_str("can't catch it without finer scopes (`--allow-fs-read PATH`). ");
    s.push_str("It's listed to show what the sandbox does *not* claim.\n\n");

    s.push_str("## Per-case detail\n\n");
    for (i, (c, r)) in results.iter().enumerate() {
        s.push_str(&format!("### {}. `{}` — {}\n\n", i + 1, c.name, c.summary));
        s.push_str(&format!("**Lex** (`--allow-effects {}`):\n\n```lex\n{}\n```\n\nVerdict: **{}**\n\n",
            if c.lex_effects.is_empty() { "(none)" } else { c.lex_effects },
            c.lex_body.trim(), r.lex.icon()));
        s.push_str(&format!("**Python**:\n\n```python\n{}\n```\n\n", c.python_code.trim()));
        s.push_str(&format!(
            "- Naive `exec` (blocklist `{}`): **{}**\n- RestrictedPython: **{}**\n\n",
            if c.python_blocklist.is_empty() { "(none)" } else { c.python_blocklist },
            r.naive.icon(), r.restricted.icon(),
        ));
    }
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(&path, s).expect("write report");
    eprintln!("→ wrote {}", path.display());
}

#[test]
fn agent_sandbox_benchmark() {
    // Skip if python3 isn't available.
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skip: python3 not installed");
        return;
    }
    let results = run_all();
    write_report(&results);

    // Per-case assertions: every adversarial case must be Blocked or Errored
    // by Lex. Benign cases must be Ran by Lex. Python's outcome is recorded
    // but not asserted (we know it's broken — that's the point).
    for (c, r) in &results {
        match c.intent {
            Intent::Adversarial => assert_ne!(
                r.lex, Verdict::Ran,
                "expected Lex to block `{}`, but it ran", c.name,
            ),
            Intent::AdversarialOutOfScope => {} // documented limitation
            Intent::Benign => assert_eq!(
                r.lex, Verdict::Ran,
                "expected Lex to run benign `{}`, got {:?}", c.name, r.lex,
            ),
        }
    }

    // Headline assertion: every targeted-adversarial case is blocked by Lex,
    // and Lex blocks at least as many as the naive Python sandbox. We do
    // *not* assert Lex strictly outperforms RestrictedPython — it doesn't,
    // and the report is honest about that. The pitch is about the *kind*
    // of guarantee, not the count.
    let attacks: Vec<_> = results.iter()
        .filter(|(c, _)| c.intent == Intent::Adversarial).collect();
    let lex_blocks   = attacks.iter().filter(|(_, r)| r.lex != Verdict::Ran).count();
    let naive_blocks = attacks.iter().filter(|(_, r)| r.naive != Verdict::Ran).count();
    assert_eq!(
        lex_blocks, attacks.len(),
        "Lex must block all targeted-adversarial cases; got {lex_blocks}/{}",
        attacks.len(),
    );
    assert!(
        lex_blocks >= naive_blocks,
        "Lex must block at least as many attacks as the naive Python sandbox; \
         got Lex={lex_blocks}/{} vs. naive={naive_blocks}/{}",
        attacks.len(), attacks.len(),
    );
}
