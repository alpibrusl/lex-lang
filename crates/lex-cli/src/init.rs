//! `lex init` — scaffold a new Lex project.
//!
//! Creates in the current directory (or the given path):
//!   lex.toml                   package manifest
//!   src/main.lex               entry-point stub
//!   tests/test_main.lex        test stub (run_all returns 0)
//!   .github/workflows/lex.yml  GitHub Actions CI workflow
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

    let files: &[(&str, &dyn Fn(&str) -> String)] = &[
        ("lex.toml",                    &lex_toml),
        ("src/main.lex",                &main_lex),
        ("tests/test_main.lex",         &test_lex),
        (".github/workflows/lex.yml",   &ci_yml),
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
    // Use the printer's canonical output so `lex fmt --check` passes immediately.
    let src = "fn main() -> Str {\n  \"hello, world\"\n}\n";
    let prog = lex_syntax::parse_source(src).expect("stub is valid lex");
    lex_syntax::print_program(&prog)
}

fn test_lex(_name: &str) -> String {
    let src = concat!(
        "import \"std.list\" as list\n\n",
        "fn run_all() -> () {\n",
        "  list.fold([], (), fn (_ :: (), _ :: ()) -> () { () })\n",
        "}\n",
    );
    let prog = lex_syntax::parse_source(src).expect("stub is valid lex");
    lex_syntax::print_program(&prog)
}

fn ci_yml(_name: &str) -> String {
    // $GITHUB_PATH is a shell variable, not a Rust format specifier.
    // We build the string without format! to avoid escaping every $.
    [
        "name: CI\n",
        "\n",
        "on:\n",
        "  push:\n",
        "    branches: [main]\n",
        "  pull_request:\n",
        "\n",
        "jobs:\n",
        "  build:\n",
        "    runs-on: ubuntu-latest\n",
        "    steps:\n",
        "      - uses: actions/checkout@v4\n",
        "\n",
        "      - name: Install Lex toolchain\n",
        "        run: |\n",
        "          git clone --depth=1 https://github.com/alpibrusl/lex-lang /tmp/lex-lang\n",
        "          cd /tmp/lex-lang && cargo build --release -p lex-cli\n",
        "          echo \"/tmp/lex-lang/target/release\" >> $GITHUB_PATH\n",
        "\n",
        "      - name: Install package dependencies\n",
        "        run: lex pkg install\n",
        "\n",
        "      - name: Type-check (strict)\n",
        "        run: lex check --strict src/main.lex\n",
        "\n",
        "      - name: Format check\n",
        "        run: lex fmt --check src/ tests/\n",
        "\n",
        "      - name: Test\n",
        "        run: lex test\n",
    ]
    .concat()
}
