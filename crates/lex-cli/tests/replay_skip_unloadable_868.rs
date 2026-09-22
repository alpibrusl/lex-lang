//! #868 end to end: `lex op replay` over a history whose parent state names a
//! stage the store can no longer load.
//!
//! Before the fix this was `error: unknown stage_id <hash>` — the whole replay
//! aborted over one missing *context* declaration. Now the declaration is
//! skipped, listed in the request and carried to the verdict; only an
//! unloadable *target* is still an error.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn lex() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lex"))
}

fn run(args: &[&str]) -> Output {
    lex()
        .args(["--output", "json"])
        .args(args)
        .output()
        .expect("run lex")
}

fn json(args: &[&str]) -> serde_json::Value {
    let out = run(args);
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!(
            "non-JSON from lex {args:?}: {e}\nstdout: {text}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    v.get("data").cloned().unwrap_or(v)
}

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    store: String,
    g_op: String,
}

/// (sig, stage) for the declaration named `name`, read from its metadata.
fn locate(store: &Path, name: &str) -> (String, String) {
    for sig in std::fs::read_dir(store.join("stages")).unwrap() {
        let sig = sig.unwrap().path();
        let imp = sig.join("implementations");
        let Ok(rd) = std::fs::read_dir(&imp) else {
            continue;
        };
        for f in rd {
            let f = f.unwrap().path();
            let fname = f.file_name().unwrap().to_string_lossy().into_owned();
            if let Some(stage) = fname.strip_suffix(".metadata.json") {
                let m: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
                if m["name"] == name {
                    return (
                        sig.file_name().unwrap().to_string_lossy().into_owned(),
                        stage.to_string(),
                    );
                }
            }
        }
    }
    panic!("no stage named {name}");
}

fn drop_ast(fx: &Fx, name: &str) -> String {
    let (sig, stage) = locate(Path::new(&fx.store), name);
    let p = fx
        .root
        .join("s/stages")
        .join(&sig)
        .join("implementations")
        .join(format!("{stage}.ast.json"));
    std::fs::remove_file(&p).unwrap_or_else(|e| panic!("removing {}: {e}", p.display()));
    stage
}

const V1: &str = "fn helper(x :: Int) -> Int { x + 1 }\nfn other(x :: Int) -> Int { x * 3 }\n";
const G: &str = "fn g(x :: Int) -> Int { helper(x) * 2 }\n";

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let src = root.join("a.lex");
    let store = root.join("s").to_string_lossy().into_owned();
    std::fs::write(&src, V1).unwrap();
    json(&[
        "publish",
        src.to_str().unwrap(),
        "--store",
        &store,
        "--activate",
    ]);
    std::fs::write(&src, format!("{V1}{G}")).unwrap();
    let v = json(&[
        "publish",
        src.to_str().unwrap(),
        "--store",
        &store,
        "--activate",
    ]);
    let g_op = v["ops"][0]["op_id"].as_str().expect("g op").to_string();
    Fx {
        _dir: dir,
        root,
        store,
        g_op,
    }
}

#[test]
fn replay_request_skips_an_unloadable_parent_stage() {
    let fx = fixture();
    let helper_stage = drop_ast(&fx, "helper");

    let out = run(&["op", "replay", &fx.g_op, "--store", &fx.store]);
    assert!(
        out.status.success(),
        "replay must not fail over an unloadable context stage (#868): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let req = json(&["op", "replay", &fx.g_op, "--store", &fx.store]);
    assert_eq!(
        req["skipped"][0]["stage_id"],
        helper_stage.as_str(),
        "{req}"
    );
    assert_eq!(req["skipped"][0]["name"], "helper", "{req}");
    assert_eq!(req["skipped"][0]["called_by_target"], true, "{req}");
    let parent = req["parent_program"].as_str().unwrap();
    assert!(
        parent.contains("fn other") && !parent.contains("fn helper"),
        "{parent}"
    );
}

#[test]
fn the_verdict_carries_the_incomplete_context() {
    let fx = fixture();
    drop_ast(&fx, "helper");
    let cand = fx.root.join("cand.lex");

    // A faithful regeneration still reproduces exactly — the target itself
    // is intact — but the verdict records that the context was partial.
    std::fs::write(&cand, G).unwrap();
    let r = json(&[
        "op",
        "replay",
        &fx.g_op,
        "--store",
        &fx.store,
        "--candidate",
        cand.to_str().unwrap(),
    ]);
    assert_eq!(r["reproduced"], true, "{r}");
    assert_eq!(r["context_incomplete"], true, "{r}");
    assert_eq!(r["skipped"][0]["name"], "helper", "{r}");

    // A differing regeneration is a miss, and says so under partial context.
    std::fs::write(&cand, "fn g(x :: Int) -> Int { helper(x) * 3 }\n").unwrap();
    let r = json(&[
        "op",
        "replay",
        &fx.g_op,
        "--store",
        &fx.store,
        "--candidate",
        cand.to_str().unwrap(),
    ]);
    assert_eq!(r["reproduced"], false, "{r}");
    assert_eq!(r["context_incomplete"], true, "{r}");
}

#[test]
fn a_clean_replay_verdict_has_no_new_keys() {
    let fx = fixture();
    let cand = fx.root.join("cand.lex");
    std::fs::write(&cand, G).unwrap();
    let r = json(&[
        "op",
        "replay",
        &fx.g_op,
        "--store",
        &fx.store,
        "--candidate",
        cand.to_str().unwrap(),
    ]);
    assert_eq!(r["reproduced"], true, "{r}");
    assert!(
        r.get("context_incomplete").is_none() && r.get("skipped").is_none(),
        "{r}"
    );
}

/// Negative control: the op's own target missing is still a hard, clear error.
#[test]
fn an_unloadable_target_still_fails_clearly() {
    let fx = fixture();
    let g_stage = drop_ast(&fx, "g");
    let out = run(&["op", "replay", &fx.g_op, "--store", &fx.store]);
    assert!(
        !out.status.success(),
        "replaying an unloadable target must fail"
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("cannot replay") && all.contains(&g_stage),
        "{all}"
    );
}
