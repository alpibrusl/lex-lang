//! Real install + archive round-trip for #1031: a registry dependency's
//! rendered archive must carry a faithful `[dependencies]` table, not just
//! its own `lex.lock`, otherwise a 2-hop chain (A -> B -> C) can `lex pkg
//! install` and `lex pkg lock` in A, yet fail `lex publish` because the
//! client resolver never learns B depends on C at all -- exactly the gap
//! #943's own tests never exercised, because those drive the hub's
//! in-process store resolver directly (`lex-store/tests/deps_recursive_943.rs`)
//! and never round-trip through a real archive download.
//!
//! Every step below is the real CLI (`lex publish`, `lex op push`,
//! `lex pkg release`, `lex pkg install`, `lex pkg lock`) against a real
//! in-process `lex-api` server -- the same handler code a production
//! `lex-hub` embeds for `POST /v1/pkg/{name}/release` and
//! `GET /v1/pkg/{name}/{version}/archive`.
//!
//! Every `publish` below passes `--no-files` (#1007 PR 4): a directory
//! publish now captures its non-op-log files into a `SetFiles` op by
//! default, and `lex op push` cannot yet sync that op's blobs (blob sync
//! lands in PR 5) -- pushing one here would 422 `MissingBlobs`. This test is
//! about registry/archive dependency resolution, not files capture, so
//! opting out keeps it isolated to what it actually exercises.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

struct Server {
    addr: SocketAddr,
    _join: Option<thread::JoinHandle<()>>,
}

fn start_server() -> (Server, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let join = thread::spawn(move || {
        lex_api::serve_on(server, state);
    });
    wait_until_serving(&addr);
    (Server { addr, _join: Some(join) }, tmp)
}

fn wait_until_serving(addr: &SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    while std::time::Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(addr, Duration::from_millis(200)) {
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            if s.write_all(probe).is_ok() {
                let mut buf = [0u8; 16];
                if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test server never became ready within 10s");
}

/// Run the real `lex` binary as a subprocess, in `cwd`, with an isolated
/// `$HOME`/`$LEX_PACKAGES_DIR` (`env_root`) so it never touches the
/// developer's real `~/.lex`.
fn run_lex(cwd: &Path, env_root: &Path, args: &[&str]) -> Output {
    Command::new(lex_bin())
        .current_dir(cwd)
        .env("HOME", env_root)
        .env("LEX_PACKAGES_DIR", env_root.join("packages"))
        .env_remove("LEX_STORE")
        .env_remove("LEXHUB_TOKEN")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning `lex {}`: {e}", args.join(" ")))
}

fn ok(cwd: &Path, env_root: &Path, args: &[&str]) -> Output {
    let out = run_lex(cwd, env_root, args);
    assert!(
        out.status.success(),
        "`lex {}` failed (cwd={}):\nstdout: {}\nstderr: {}",
        args.join(" "),
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn write_pkg(dir: &Path, name: &str, version: &str, deps_toml: &str, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let manifest = format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n{deps_toml}");
    std::fs::write(dir.join("lex.toml"), manifest).unwrap();
    for (path, body) in files {
        std::fs::write(dir.join("src").join(path), body).unwrap();
    }
}

const C_THING: &str = "type Thing = { n :: Int }\n\nfn make(n :: Int) -> Thing { { n: n } }\n";
const B_WRAP: &str = "import \"m1verify-c/thing\" as c\n\ntype Wrapper = { t :: c.Thing }\n\n\
fn wrap(n :: Int) -> c.Thing { c.make(n + 1) }\n\n\
fn boxed(n :: Int) -> Wrapper { { t: c.make(n) } }\n";
const A_MAIN: &str = "import \"m1verify-b/wrap\" as b\n\nfn use_it() -> Int { b.wrap(3).n }\n";

/// Publish (writes the local store), push (syncs to `hub`) and release (cuts
/// an immutable version) one package on `hub`'s default branch.
fn publish_push_release(dir: &Path, env_root: &Path, hub: &str, version: &str) {
    ok(dir, env_root, &["publish", "--no-files", "."]);
    ok(dir, env_root, &["op", "push", hub]);
    ok(dir, env_root, &["pkg", "release", hub, "--version", version, "--token", "t"]);
}

#[test]
fn a_locally_publishes_against_a_two_hop_registry_dependency() {
    // Three real packages, each hosted on its own hub-like server -- the
    // same shape `lex-store/tests/deps_recursive_943.rs` uses ("each in its
    // own store"), except here every hop is a REAL `lex pkg install` +
    // archive download over HTTP, not the hub's in-process store resolver.
    let (c_server, _c_hub_tmp) = start_server();
    let (b_server, _b_hub_tmp) = start_server();
    let c_hub = format!("http://{}", c_server.addr);
    let b_hub = format!("http://{}", b_server.addr);
    let env_root = TempDir::new().unwrap();

    let work = TempDir::new().unwrap();
    let c_dir = work.path().join("c");
    let b_dir = work.path().join("b");
    let a_dir = work.path().join("a");

    // C: leaf, no dependencies.
    write_pkg(&c_dir, "m1verify-c", "0.1.0", "", &[("thing.lex", C_THING)]);
    publish_push_release(&c_dir, env_root.path(), &c_hub, "0.1.0");

    // B: depends on C by exact version -- resolvable from B's own working
    // copy without a lock, but `lex pkg lock` still runs (matching the
    // issue's repro) and is what `lex publish` commits into B's store head.
    write_pkg(
        &b_dir,
        "m1verify-b",
        "0.2.0",
        &format!("\n[dependencies]\nm1verify-c = {{ registry = \"{c_hub}\", version = \"0.1.0\" }}\n"),
        &[("wrap.lex", B_WRAP)],
    );
    ok(&b_dir, env_root.path(), &["pkg", "install"]);
    ok(&b_dir, env_root.path(), &["pkg", "lock"]);
    publish_push_release(&b_dir, env_root.path(), &b_hub, "0.2.0");

    // A: depends on B ONLY. C is two hops away -- never named in A's own
    // lex.toml or lex.lock, resolvable only through B's own shipped
    // manifest+lock.
    write_pkg(
        &a_dir,
        "m1verify-a",
        "0.3.0",
        &format!("\n[dependencies]\nm1verify-b = {{ registry = \"{b_hub}\", version = \"0.2.0\" }}\n"),
        &[("lib.lex", A_MAIN)],
    );
    ok(&a_dir, env_root.path(), &["pkg", "install"]);
    ok(&a_dir, env_root.path(), &["pkg", "lock"]);

    // This is the exact reproduction from #1031: `lex publish .` on A
    // exercises the client resolver (`lex-cli/src/dep_resolver.rs`) named
    // there as the root cause.
    let out = run_lex(&a_dir, env_root.path(), &["publish", "--no-files", "."]);
    assert!(
        out.status.success(),
        "A's `lex publish .` must succeed against a real 2-hop registry \
         dependency chain (A -> B -> C):\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// The precise state #1031 describes: B's *release record* carries an empty
/// `dependency_specs` (e.g. an immutable release cut before a server started
/// capturing full dependency coordinates, or by an older client that never
/// sent them) even though B's *committed lock* — sitting right next to the
/// synthesized `lex.toml` in the same rendered archive / cache directory —
/// still pins C. `lex publish .` commits the lock into B's store head
/// *before* release (so the lock is genuinely present at the head being
/// released); stripping `[dependencies]` from B's manifest right before
/// `lex pkg release` (which re-reads the manifest fresh) reproduces an empty
/// `dependency_specs` without needing an old server/client binary.
///
/// Without the client-side fallback (#1031), this must fail with the exact
/// error the issue reports: `resolve_package_import`'s `UnknownPackage`,
/// "package \"m1verify-c\" not found in .../lex.toml". With it, A's local
/// `lex publish .` recovers C's coordinates from B's sibling `lex.lock` and
/// succeeds. This is the negative control: run it against an unpatched
/// checkout to see the original failure; the fix flips it to a pass.
#[test]
fn a_locally_publishes_when_bs_archive_strips_the_dependencies_table() {
    let (c_server, _c_hub_tmp) = start_server();
    let (b_server, _b_hub_tmp) = start_server();
    let c_hub = format!("http://{}", c_server.addr);
    let b_hub = format!("http://{}", b_server.addr);
    let env_root = TempDir::new().unwrap();

    let work = TempDir::new().unwrap();
    let c_dir = work.path().join("c");
    let b_dir = work.path().join("b");
    let a_dir = work.path().join("a");

    write_pkg(&c_dir, "m1verify-c", "0.1.0", "", &[("thing.lex", C_THING)]);
    publish_push_release(&c_dir, env_root.path(), &c_hub, "0.1.0");

    let b_toml = b_dir.join("lex.toml");
    write_pkg(
        &b_dir,
        "m1verify-b",
        "0.2.0",
        &format!("\n[dependencies]\nm1verify-c = {{ registry = \"{c_hub}\", version = \"0.1.0\" }}\n"),
        &[("wrap.lex", B_WRAP)],
    );
    ok(&b_dir, env_root.path(), &["pkg", "install"]);
    ok(&b_dir, env_root.path(), &["pkg", "lock"]);
    // Commits B's lex.lock (pinning C) into B's store head.
    ok(&b_dir, env_root.path(), &["publish", "--no-files", "."]);
    ok(&b_dir, env_root.path(), &["op", "push", &b_hub]);
    // Strip [dependencies] from the manifest `pkg release` is about to
    // re-read — simulating a release whose captured `dependency_specs` came
    // back empty, with no code change and no need for an old binary. The
    // already-pushed head (and its already-committed lock) are unaffected.
    std::fs::write(&b_toml, "[package]\nname = \"m1verify-b\"\nversion = \"0.2.0\"\n").unwrap();
    ok(&b_dir, env_root.path(), &["pkg", "release", &b_hub, "--version", "0.2.0", "--token", "t"]);

    write_pkg(
        &a_dir,
        "m1verify-a",
        "0.3.0",
        &format!("\n[dependencies]\nm1verify-b = {{ registry = \"{b_hub}\", version = \"0.2.0\" }}\n"),
        &[("lib.lex", A_MAIN)],
    );
    ok(&a_dir, env_root.path(), &["pkg", "install"]);
    ok(&a_dir, env_root.path(), &["pkg", "lock"]);

    // `lex check <file>` always inlines package imports
    // (`lex-syntax/src/loader.rs::load_rooted`, `inline_packages: true`) and
    // propagates `resolve_package_import`'s error directly — unlike the
    // publish-time `ClientDepResolver`, which swallows a dependency that
    // fails to resolve (leaving its references merely unbound, reported as
    // `unknown_identifier` instead). This is what reproduces #1031's exact
    // reported error text.
    let out = run_lex(&a_dir, env_root.path(), &["check", "--strict", "src/lib.lex"]);
    assert!(
        out.status.success(),
        "A's `lex check` must recover C's coordinates from B's sibling \
         lex.lock when B's archive carries no [dependencies] table:\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // And the full publish path (the other command #1031 names) must also
    // succeed, not just report a different, more confusing symptom.
    let out = run_lex(&a_dir, env_root.path(), &["publish", "--no-files", "."]);
    assert!(
        out.status.success(),
        "A's `lex publish .` must recover C's coordinates from B's sibling \
         lex.lock when B's archive carries no [dependencies] table:\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
