//! `lex pkg install` refuses a dependency that needs a newer toolchain (#803).
//!
//! Every package in this org declares `lex = "X.Y.Z"` in its manifest,
//! and until #803 nothing read it — serde drops unknown fields, so the
//! floor was decoration. A dependency that had moved onto a newer
//! stdlib installed without complaint into a project pinned older, and
//! the mismatch was felt much later as `unknown_field` type errors
//! naming stdlib functions the consuming repo never called. Found in
//! lex-gateway: 24 of 35 files failing `lex check`, and the only clue
//! sat unread in a dependency's manifest.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

/// A consumer project plus one path dependency declaring `floor`.
/// Returns the consumer dir and a private package cache.
fn scenario(tag: &str, floor: Option<&str>) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("lex-floor-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let dep = root.join("dep");
    let consumer = root.join("consumer");
    let cache = root.join("pkgs");
    fs::create_dir_all(dep.join("src")).unwrap();
    fs::create_dir_all(&consumer).unwrap();

    let floor_line = floor
        .map(|f| format!("lex     = \"{f}\"\n"))
        .unwrap_or_default();
    fs::write(
        dep.join("lex.toml"),
        format!("[package]\nname    = \"floordep\"\nversion = \"0.1.0\"\n{floor_line}"),
    )
    .unwrap();
    fs::write(dep.join("src/lib.lex"), "fn hello() -> Str { \"hi\" }\n").unwrap();
    fs::write(
        consumer.join("lex.toml"),
        "[package]\nname    = \"consumer\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\nfloordep = { path = \"../dep\" }\n",
    )
    .unwrap();
    (consumer, cache)
}

fn install(dir: &Path, cache: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(lex_bin())
        .arg("pkg")
        .arg("install")
        .args(args)
        .current_dir(dir)
        .env("LEX_PACKAGES_DIR", cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

#[test]
fn a_dependency_needing_a_newer_toolchain_is_refused() {
    let (dir, cache) = scenario("unmet", Some("99.0.0"));
    let (code, out) = install(&dir, &cache, &[]);
    assert_ne!(code, 0, "install must fail; output:\n{out}");
    assert!(
        out.contains("floordep requires lex >= 99.0.0"),
        "must name the package and its floor:\n{out}"
    );
    assert!(
        out.contains(env!("CARGO_PKG_VERSION")),
        "must name the toolchain actually running:\n{out}"
    );
}

#[test]
fn a_satisfied_floor_is_silent() {
    let (dir, cache) = scenario("met", Some("0.0.1"));
    let (code, out) = install(&dir, &cache, &[]);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(!out.contains("requires lex >="), "output:\n{out}");
}

/// The overwhelmingly common case, and the one that must not regress:
/// a manifest with no floor at all behaves exactly as before.
#[test]
fn no_declared_floor_installs_as_before() {
    let (dir, cache) = scenario("absent", None);
    let (code, out) = install(&dir, &cache, &[]);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(out.contains("installed 1 package(s)"), "output:\n{out}");
}

#[test]
fn the_escape_hatch_installs_but_still_says_so() {
    let (dir, cache) = scenario("hatch", Some("99.0.0"));
    let (code, out) = install(&dir, &cache, &["--ignore-lex-floor"]);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(out.contains("installed 1 package(s)"), "output:\n{out}");
    assert!(
        out.contains("floordep requires lex >= 99.0.0"),
        "installing past a floor must still be reported:\n{out}"
    );
}

/// A floor this tool cannot parse is reported as unchecked. Treating it
/// as met would quietly defeat the check; treating it as violated would
/// fail installs that were fine.
#[test]
fn an_unparseable_floor_warns_and_installs() {
    let (dir, cache) = scenario("weird", Some("nightly"));
    let (code, out) = install(&dir, &cache, &[]);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(out.contains("not a MAJOR.MINOR.PATCH"), "output:\n{out}");
}
