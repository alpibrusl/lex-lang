//! `lex init` — scaffold a new Lex project.
//!
//! Creates in the current directory (or the given path):
//!   lex.toml                   package manifest
//!   src/main.lex               entry-point stub (with an `examples { }` block)
//!   tests/test_main.lex        test stub (`run_all` returns 0)
//!   .github/workflows/lex.yml  GitHub Actions CI workflow
//!   AGENTS.md                  cold-start guide for AI assistants
//!
//! Existing files are never overwritten.

use anyhow::{Context, Result};
use std::path::Path;

pub fn cmd_init(args: &[String]) -> Result<()> {
    let root = args.first().map(|s| s.as_str()).unwrap_or(".");
    let root = Path::new(root);

    if !root.exists() {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating directory {}", root.display()))?;
    }

    let name = root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "my-project".to_string());

    let mut created = Vec::new();
    let mut skipped = Vec::new();

    type Gen = fn(&str) -> String;
    let files: &[(&str, Gen)] = &[
        ("lex.toml",                    lex_toml),
        ("src/main.lex",                main_lex),
        ("tests/test_main.lex",         test_lex),
        (".github/workflows/lex.yml",   ci_yml),
        ("AGENTS.md",                   agents_md),
    ];

    for (rel, gen) in files {
        let path = root.join(rel);
        if path.exists() {
            skipped.push(rel.to_string());
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, gen(&name))
            .with_context(|| format!("writing {}", path.display()))?;
        created.push(rel.to_string());
    }

    for f in &created { println!("  created  {f}"); }
    for f in &skipped { println!("  skipped  {f}  (already exists)"); }

    if !created.is_empty() {
        println!("\nproject `{name}` initialized. next steps:");
        println!("  lex check src/main.lex");
        println!("  lex test");
        println!("  lex fmt src/");
    }

    Ok(())
}

fn lex_toml(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"

[dependencies]
# lex-schema = {{ path = "../lex-schema" }}
# lex-schema = {{ git = "https://github.com/alpibrusl/lex-schema" }}
"#
    )
}

fn main_lex(_name: &str) -> String {
    // Includes an `examples { ... }` block so the convention is visible
    // from line 1 of a fresh repo — agents pattern-match what they see,
    // and these double as type-level + behavioural regression checks at
    // `lex check` time.
    //
    // Run through the printer so `lex fmt --check` is green on a
    // freshly-initialized project.
    let src = concat!(
        "fn main() -> Str\n",
        "  examples {\n",
        "    main() => \"hello, world\",\n",
        "  }\n",
        "{\n",
        "  \"hello, world\"\n",
        "}\n",
    );
    let prog = lex_syntax::parse_source(src).expect("stub is valid lex");
    lex_syntax::print_program(&prog)
}

fn test_lex(_name: &str) -> String {
    // `lex test` invokes `run_all` and treats a successful return as
    // pass. Convention: return `0` for "no failures" and add real cases
    // as you grow the suite. Keeping the stub trivially compiling means
    // `lex ci` is green from minute one on a freshly-initialized
    // project — agents reading AGENTS.md trust that as the baseline.
    let src = concat!(
        "fn run_all() -> Int\n",
        "  examples {\n",
        "    run_all() => 0,\n",
        "  }\n",
        "{\n",
        "  0\n",
        "}\n",
    );
    let prog = lex_syntax::parse_source(src).expect("stub is valid lex");
    lex_syntax::print_program(&prog)
}

fn ci_yml(_name: &str) -> String {
    // Pin to the lex version that scaffolded the project so CI is
    // reproducible. Bump manually (`LEX_VERSION` env in the workflow)
    // when you upgrade the toolchain.
    //
    // We download the pre-built binary from GitHub Releases instead of
    // `cargo build`-ing — ~30s vs ~3min, no Rust required.
    // `$GITHUB_PATH` is a shell variable, not a Rust format specifier;
    // we build with `format!` and escape every `{`/`}` accordingly.
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"name: CI

on:
  push:
    branches: [main]
  pull_request:

env:
  # Pinned at scaffold time. Bump when you upgrade the toolchain.
  LEX_VERSION: v{version}

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Lex toolchain (pre-built binary)
        run: |
          set -euo pipefail
          TARGET=x86_64-unknown-linux-gnu
          URL="https://github.com/alpibrusl/lex-lang/releases/download/${{LEX_VERSION}}/lex-${{LEX_VERSION}}-${{TARGET}}.tar.gz"
          curl -sSfL "$URL" | tar -xz
          sudo install -m 0755 "lex-${{LEX_VERSION}}-${{TARGET}}/lex" /usr/local/bin/lex
          lex --version

      - name: Install package dependencies
        run: lex pkg install

      - name: Type-check (strict)
        run: lex check --strict src/main.lex

      - name: Format check
        run: lex fmt --check src/ tests/

      - name: Test
        run: lex test

      # Belt-and-braces: re-run the same checks via the `lex ci`
      # umbrella so this workflow stays in sync with whatever
      # `lex ci` runs locally (contributors run `lex ci` before
      # pushing). Remove if you find the duplication too noisy.
      - name: lex ci (full repro)
        run: lex ci
"#,
        version = version,
    )
}

/// AI-assistant cold-start guide dropped at the project root. Read by
/// Claude Code (`CLAUDE.md`/`AGENTS.md`), Cursor, Aider, and most other
/// agent tools that look for a project-conventions file. Deliberately
/// short — points at `lex agent-guidelines` for the authoritative
/// prescriptive rules, and at the upstream `docs/AGENT.md` for the
/// reference deep dive.
fn agents_md(name: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"# AGENTS.md — {name}

This file is for AI assistants (Claude Code, Cursor, Aider, Copilot, …)
working in this repo. Humans should read `README.md` first; agents should
read this **first**, then run `lex agent-guidelines` for the authoritative
idiom contract before writing any code.

## 1. Install the Lex toolchain

If `lex --version` doesn't work, download the pre-built binary for your
platform. This project was scaffolded against **v{version}** — the CI
workflow is pinned to that version, so use it locally too.

```sh
LEX_VERSION=v{version}
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)   TARGET=x86_64-unknown-linux-gnu  ;;
  Linux-aarch64)  TARGET=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64)  TARGET=x86_64-apple-darwin       ;;
  Darwin-arm64)   TARGET=aarch64-apple-darwin      ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac
curl -sSfL "https://github.com/alpibrusl/lex-lang/releases/download/${{LEX_VERSION}}/lex-${{LEX_VERSION}}-${{TARGET}}.tar.gz" | tar -xz
sudo install -m 0755 "lex-${{LEX_VERSION}}-${{TARGET}}/lex" /usr/local/bin/lex
lex --version
```

Windows: download `lex-v{version}-x86_64-pc-windows-msvc.zip` from
<https://github.com/alpibrusl/lex-lang/releases/tag/v{version}> and put
`lex.exe` on `PATH`.

**Fallback (build from source).** Only needed if you want a version
that has no published release yet (off-`main` fixes, custom branches):

```sh
git clone --depth=1 https://github.com/alpibrusl/lex-lang /tmp/lex-lang
cd /tmp/lex-lang && cargo build --release -p lex-cli
export PATH="/tmp/lex-lang/target/release:$PATH"
```

Requires Rust 1.80+. Takes ~3 minutes the first time.

## 2. The loop

Every change goes through this loop. **Do not claim done before `lex ci`
is green.**

```sh
lex check src/main.lex   # type-check (fast, catches most issues)
lex test                 # run all tests/test_*.lex files
lex fmt src/ tests/      # auto-format (or `lex fmt --check` to verify)
lex ci                   # umbrella: pkg install + check --strict + fmt --check + test
```

`lex check --output json` emits structured errors with `rule_tag`,
`position`, and `rule_explanation` fields — use these when iterating.

## 3. Lex in 60 seconds (the bits most likely to trip you up)

Coming from Rust / TypeScript / Python? These are the differences worth
internalising before writing your first line:

```lex
import "std.list" as list           # stdlib import, alias is mandatory
import "./helper" as h              # local import (path relative to this file)

type Status = Healthy | Sick(Str)   # tagged union, no `enum` keyword

fn parse(s :: Str) -> Result[Int, Str]   # `::` types params; `->` is the return arrow
  examples {{                        # OPTIONAL: pure fns can carry test cases
    parse("1") => Ok(1),
    parse("x") => Err("not a number"),
  }}
{{
  let n := str.length(s)            # `:=` for let-binding, NOT `=`
  if n == 0 {{
    Err("empty")
  }} else {{
    Ok(n)
  }}
}}

fn save(path :: Str, body :: Str) -> [fs.write] Result[Unit, Str] {{
  fs.write(path, body)              # `[effects]` between `->` and the type
}}
```

Key rules:

1. **Types use `::`, lets use `:=`, returns use `->`.** Easy to mix up; the
   compiler error is clear when you do.
2. **Effects are types.** Any function that does I/O, time, randomness,
   network, LLM calls, etc. must declare them: `-> [io] Nil`,
   `-> [http.get, fs.read] Result[Str, Str]`. Pure functions declare
   nothing. The checker refuses bodies that reach outside their declaration.
3. **No exceptions.** `Result[T, E]` and `Option[T]` are the only error /
   absence channels. Idiom: `match res {{ Ok(x) => ..., Err(e) => ... }}`.
4. **`examples {{ … }}` blocks are part of the signature.** They're
   compiled into the canonical AST and run at `lex check` time. Use them
   for every pure function — they're cheaper than a test and they survive
   refactors.
5. **No mutation in user code.** No `mut`, no `var`. Build new values.
6. **One canonical AST per meaning.** `lex fmt` is deterministic; don't
   fight it.

## 4. This project

- Source lives in `src/` — entry point is `src/main.lex`.
- Tests live in `tests/` — files must start with `test_` and export
  `fn run_all() -> ...`.
- Dependencies go in `[dependencies]` of `lex.toml`; run `lex pkg install`
  after editing.
- Before pushing: `lex ci`. CI runs the same command.

## 5. Idiom rules — read before writing code

Run this in the project root:

```sh
lex agent-guidelines               # full prescriptive contract (~10 pages)
lex agent-guidelines > AGENTS.md   # capture into the repo so it travels with the code
```

The rules are numbered and stable. The four that matter most when
you're tempted to skip them:

1. **Narrow effects, always.** `fn foo() -> [fs_write("/tmp/x")] T`,
   not `[fs_write]`. If the type checker rejects, narrow the *body*,
   not the signature. Rule 1.2 in the guidelines.
2. **Repair, don't regenerate.** When `lex check` fails, run
   `lex --output json check` to get the structured error, then
   `lex repair --apply --transform '<suggested_transform>'`. Only
   regenerate after two failed repair attempts. Rules 4.1–4.3.
3. **`examples {{}}` blocks on every pure fn.** They're part of the
   SigId and run at `lex check` time — free regression tests with no
   `tests/` boilerplate. Rule 2.1.
4. **Use the stdlib.** `std.crypto` not hand-rolled crypto, `std.conc`
   not threads, `std.sql` not string-concat SQL. Section 3.

## 6. Need more?

Deep references in the upstream repo:

- **`lex agent-guidelines`** — the authoritative idiom contract
  (travels with the toolchain version).
- **`docs/AGENT.md`** — reference: error envelope schema, every
  `rule_tag`, stdlib module summary, every sharp edge.
- **`docs/design/canonicalization.md`** — which edits preserve a
  SigId and which break it.
- **`docs/index.html`** — the design pitch (effects-as-types,
  content-addressed AST, op log, attestations).
- **`README.md`** in [alpibrusl/lex-lang](https://github.com/alpibrusl/lex-lang)
  — design rules, stdlib index, examples.

When a `lex check` error confuses you, search its `rule_tag` in
`docs/AGENT.md` — most tags have an explanation and a fix template.
"#,
        name = name,
    )
}
