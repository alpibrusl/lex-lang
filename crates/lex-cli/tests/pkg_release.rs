//! `lex pkg release` end to end (#893): cut a versioned release of the hosted
//! head, with the version + dependency edges read from lex.toml, against a
//! real lex-api server.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

fn start_server() -> (SocketAddr, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(50));
    (addr, tmp)
}

fn run(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(lex_bin()).args(args).current_dir(cwd).output().unwrap()
}

#[test]
fn pkg_release_reads_version_and_deps_from_lex_toml() {
    let (addr, _srv) = start_server();
    let url = format!("http://{addr}");

    // A package declaring a version and a registry dependency.
    let tmp = TempDir::new().unwrap();
    let pkg = tmp.path();
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("lex.toml"),
        "[package]\nname = \"relcli\"\nversion = \"2.0.0\"\n\n\
         [dependencies]\nlex-nt = { registry = \"h/lex-official/lex-nt\", version = \"^1\" }\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/lib.lex"), "fn f(x :: Int) -> Int { x }\n").unwrap();

    // Publish locally + push to the server so it has a head to release.
    let store = pkg.join(".lex/store");
    assert!(run(&["publish", "--store", store.to_str().unwrap(), "--activate", "src/lib.lex"], pkg).status.success());
    assert!(run(&["op", "push", &url, "--store", store.to_str().unwrap()], pkg).status.success(), "push");

    // Release via the CLI — version + deps come from lex.toml.
    let out = run(&["pkg", "release", &url, "--token", "unused"], pkg);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "release: {so}{}", String::from_utf8_lossy(&out.stderr));
    assert!(so.contains("relcli@2.0.0"), "reports name@version: {so}");
    assert!(so.contains("lex-nt"), "reports the dep edge: {so}");

    // The server's release record carries the version and the declared dep.
    let body = ureq::get(&format!("{url}/v1/pkg/relcli/2.0.0"))
        .call().unwrap().into_body().read_to_string().unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["version"], "2.0.0");
    let deps: Vec<String> = v["dependencies"].as_array().unwrap()
        .iter().map(|d| d.as_str().unwrap().to_string()).collect();
    assert!(deps.contains(&"lex-nt".to_string()), "dep edge recorded: {body}");

    // Re-release the same version → the CLI surfaces the 409 immutability.
    let out = run(&["pkg", "release", &url, "--token", "unused"], pkg);
    assert!(!out.status.success(), "re-release must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("immutable"), "409 surfaced");
}
