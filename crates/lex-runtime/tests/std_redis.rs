//! Integration tests for `std.redis` (#533).
//!
//! The full round-trip tests (connect, get/set, publish/subscribe, etc.)
//! require a live Redis server. Those tests are skipped when
//! `REDIS_TEST_URL` is not set; the remaining tests exercise type-checking,
//! policy enforcement, and error-path wiring without a real server.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

fn policy_with_net() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["net".to_string()].into_iter().collect::<BTreeSet<_>>();
    p
}

fn run(src: &str, func: &str, args: Vec<Value>, policy: Policy) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

fn redis_url() -> Option<String> {
    std::env::var("REDIS_TEST_URL").ok()
}

fn unwrap_ok(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Ok" => args.into_iter().next().expect("Ok payload"),
        other => panic!("expected Ok, got {other:?}"),
    }
}

fn unwrap_err_str(v: Value) -> String {
    match v {
        Value::Variant { name, args } if name == "Err" => match args.into_iter().next() {
            Some(Value::Str(s)) => s.to_string(),
            other => panic!("expected Err(Str), got {other:?}"),
        },
        other => panic!("expected Err, got {other:?}"),
    }
}

// ── Type-checking smoke tests ─────────────────────────────────────────────
//
// These programs must parse and type-check cleanly regardless of whether a
// Redis server is available. They cover every op in the surface API.

const TYPE_CHECK_SRC: &str = r#"
import "std.redis" as redis

fn check_connect(url :: Str) -> [net] Result[ConnRedis, Str] {
  redis.connect(url)
}

fn check_close(conn :: ConnRedis) -> [net] Unit {
  redis.close(conn)
}

fn check_get(conn :: ConnRedis, key :: Str) -> [net] Option[Str] {
  redis.get(conn, key)
}

fn check_set(conn :: ConnRedis, key :: Str, val :: Str) -> [net] Unit {
  redis.set(conn, key, val)
}

fn check_set_ex(conn :: ConnRedis, key :: Str, val :: Str, ttl :: Int) -> [net] Unit {
  redis.set_ex(conn, key, val, ttl)
}

fn check_del(conn :: ConnRedis, key :: Str) -> [net] Unit {
  redis.del(conn, key)
}

fn check_exists(conn :: ConnRedis, key :: Str) -> [net] Bool {
  redis.exists(conn, key)
}

fn check_expire(conn :: ConnRedis, key :: Str, ttl :: Int) -> [net] Unit {
  redis.expire(conn, key, ttl)
}

fn check_publish(conn :: ConnRedis, ch :: Str, msg :: Str) -> [net] Int {
  redis.publish(conn, ch, msg)
}

fn check_lpush(conn :: ConnRedis, key :: Str, val :: Str) -> [net] Int {
  redis.lpush(conn, key, val)
}

fn check_rpush(conn :: ConnRedis, key :: Str, val :: Str) -> [net] Int {
  redis.rpush(conn, key, val)
}

fn check_brpop(conn :: ConnRedis, key :: Str, timeout :: Int) -> [net] Option[Str] {
  redis.brpop(conn, key, timeout)
}

fn check_llen(conn :: ConnRedis, key :: Str) -> [net] Int {
  redis.llen(conn, key)
}

fn check_hset(conn :: ConnRedis, key :: Str, field :: Str, val :: Str) -> [net] Unit {
  redis.hset(conn, key, field, val)
}

fn check_hget(conn :: ConnRedis, key :: Str, field :: Str) -> [net] Option[Str] {
  redis.hget(conn, key, field)
}

fn check_hdel(conn :: ConnRedis, key :: Str, field :: Str) -> [net] Unit {
  redis.hdel(conn, key, field)
}

fn check_hgetall(conn :: ConnRedis, key :: Str) -> [net] List[(Str, Str)] {
  redis.hgetall(conn, key)
}
"#;

/// Confirm the entire std.redis surface type-checks without errors.
#[test]
fn all_signatures_type_check() {
    let prog = parse_source(TYPE_CHECK_SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors in std.redis surface:\n{errs:#?}");
    }
}

// ── Error-path tests (no server required) ─────────────────────────────────

const CONNECT_SRC: &str = r#"
import "std.redis" as redis

fn try_connect(url :: Str) -> [net] Result[ConnRedis, Str] {
  redis.connect(url)
}
"#;

/// Connecting to a URL that refuses the connection should surface as Err(Str),
/// not a VM crash. Uses a port that's almost certainly not Redis.
#[test]
fn connect_to_unreachable_host_returns_err() {
    let v = run(
        CONNECT_SRC,
        "try_connect",
        vec![Value::Str("redis://127.0.0.1:1".into())],
        policy_with_net(),
    );
    let msg = unwrap_err_str(v);
    assert!(
        msg.starts_with("redis.connect:"),
        "expected redis.connect: prefix, got `{msg}`",
    );
}

/// The runtime must honor the `--allow-net-host` policy for Redis URLs.
#[test]
fn connect_outside_net_host_policy_returns_err() {
    let mut policy = policy_with_net();
    policy.allow_net_host = vec!["allowed.example.com".to_string()];
    let prog = parse_source(CONNECT_SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    // Use a host NOT in the allow-list.
    let r = vm.call(
        "try_connect",
        vec![Value::Str("redis://blocked.example.com:6379".into())],
    );
    // Policy violations surface as VM-level Err (the dispatch returns Err
    // out-of-band, same shape as fs-write scope violations).
    assert!(r.is_err(), "expected policy Err, got {r:?}");
    let msg = format!("{:?}", r.unwrap_err());
    assert!(
        msg.contains("blocked.example.com"),
        "error should mention the blocked host, got `{msg}`",
    );
}

/// After redis.close the handle is invalid; further ops return VM-level errors.
#[test]
fn ops_on_closed_handle_error() {
    // We need a live connection to open a handle. Skip if no server.
    let Some(url) = redis_url() else { return; };

    let src = r#"
import "std.redis" as redis

fn open_close_then_get(url :: Str) -> [net] Option[Str] {
  match redis.connect(url) {
    Ok(conn) => {
      let _ := redis.close(conn)
      redis.get(conn, "k")
    },
    Err(_) => None,
  }
}
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy_with_net()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let r = vm.call("open_close_then_get", vec![Value::Str(url.into())]);
    assert!(r.is_err(), "expected closed-handle error, got {r:?}");
    let msg = format!("{:?}", r.unwrap_err());
    assert!(
        msg.contains("closed or unknown ConnRedis handle"),
        "expected closed-handle message, got `{msg}`",
    );
}

// ── Live round-trip tests (require REDIS_TEST_URL) ────────────────────────

const ROUND_TRIP_SRC: &str = r#"
import "std.redis" as redis

fn set_get(url :: Str, key :: Str, val :: Str) -> [net] Option[Str] {
  match redis.connect(url) {
    Ok(conn) => {
      let _ := redis.set(conn, key, val)
      redis.get(conn, key)
    },
    Err(_) => None,
  }
}

fn get_missing(url :: Str, key :: Str) -> [net] Option[Str] {
  match redis.connect(url) {
    Ok(conn) => redis.get(conn, key),
    Err(_)   => None,
  }
}

fn del_then_exists(url :: Str, key :: Str) -> [net] Bool {
  match redis.connect(url) {
    Ok(conn) => {
      let _ := redis.set(conn, key, "x")
      let _ := redis.del(conn, key)
      redis.exists(conn, key)
    },
    Err(_) => true,
  }
}

fn set_ex_then_exists(url :: Str, key :: Str) -> [net] Bool {
  match redis.connect(url) {
    Ok(conn) => {
      let _ := redis.set_ex(conn, key, "v", 60)
      redis.exists(conn, key)
    },
    Err(_) => false,
  }
}

fn publish_returns_int(url :: Str, ch :: Str) -> [net] Bool {
  match redis.connect(url) {
    Ok(conn) => {
      let n := redis.publish(conn, ch, "hello")
      n >= 0
    },
    Err(_) => false,
  }
}

fn list_roundtrip(url :: Str, key :: Str) -> [net] Int {
  match redis.connect(url) {
    Ok(conn) => {
      let _ := redis.del(conn, key)
      let _ := redis.lpush(conn, key, "a")
      let _ := redis.rpush(conn, key, "b")
      redis.llen(conn, key)
    },
    Err(_) => 0 - 1,
  }
}

fn hash_roundtrip(url :: Str, key :: Str) -> [net] Option[Str] {
  match redis.connect(url) {
    Ok(conn) => {
      let _ := redis.del(conn, key)
      let _ := redis.hset(conn, key, "field1", "value1")
      redis.hget(conn, key, "field1")
    },
    Err(_) => None,
  }
}

fn hash_del_then_get(url :: Str, key :: Str) -> [net] Option[Str] {
  match redis.connect(url) {
    Ok(conn) => {
      let _ := redis.del(conn, key)
      let _ := redis.hset(conn, key, "f", "v")
      let _ := redis.hdel(conn, key, "f")
      redis.hget(conn, key, "f")
    },
    Err(_) => None,
  }
}
"#;

fn unique_key(tag: &str) -> String {
    format!(
        "lex-test:{}-{}-{}",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[test]
fn set_get_round_trip() {
    let Some(url) = redis_url() else { return; };
    let key = unique_key("set-get");
    let v = run(
        ROUND_TRIP_SRC,
        "set_get",
        vec![
            Value::Str(url.into()),
            Value::Str(key.into()),
            Value::Str("hello-redis".into()),
        ],
        policy_with_net(),
    );
    assert_eq!(
        v,
        Value::Variant {
            name: "Some".into(),
            args: vec![Value::Str("hello-redis".into())],
        },
        "expected Some(\"hello-redis\"), got {v:?}",
    );
}

#[test]
fn get_missing_returns_none() {
    let Some(url) = redis_url() else { return; };
    let key = unique_key("get-missing");
    let v = run(
        ROUND_TRIP_SRC,
        "get_missing",
        vec![Value::Str(url.into()), Value::Str(key.into())],
        policy_with_net(),
    );
    assert_eq!(v, Value::Variant { name: "None".into(), args: vec![] });
}

#[test]
fn del_removes_key() {
    let Some(url) = redis_url() else { return; };
    let key = unique_key("del");
    let v = run(
        ROUND_TRIP_SRC,
        "del_then_exists",
        vec![Value::Str(url.into()), Value::Str(key.into())],
        policy_with_net(),
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn set_ex_key_exists() {
    let Some(url) = redis_url() else { return; };
    let key = unique_key("set-ex");
    let v = run(
        ROUND_TRIP_SRC,
        "set_ex_then_exists",
        vec![Value::Str(url.into()), Value::Str(key.into())],
        policy_with_net(),
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn publish_returns_non_negative() {
    let Some(url) = redis_url() else { return; };
    let v = run(
        ROUND_TRIP_SRC,
        "publish_returns_int",
        vec![
            Value::Str(url.into()),
            Value::Str(unique_key("publish-ch").into()),
        ],
        policy_with_net(),
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn lpush_rpush_llen() {
    let Some(url) = redis_url() else { return; };
    let key = unique_key("list");
    let v = run(
        ROUND_TRIP_SRC,
        "list_roundtrip",
        vec![Value::Str(url.into()), Value::Str(key.into())],
        policy_with_net(),
    );
    assert_eq!(v, Value::Int(2));
}

#[test]
fn hset_hget_round_trip() {
    let Some(url) = redis_url() else { return; };
    let key = unique_key("hash");
    let v = run(
        ROUND_TRIP_SRC,
        "hash_roundtrip",
        vec![Value::Str(url.into()), Value::Str(key.into())],
        policy_with_net(),
    );
    assert_eq!(
        v,
        Value::Variant {
            name: "Some".into(),
            args: vec![Value::Str("value1".into())],
        },
    );
}

#[test]
fn hdel_removes_field() {
    let Some(url) = redis_url() else { return; };
    let key = unique_key("hash-del");
    let v = run(
        ROUND_TRIP_SRC,
        "hash_del_then_get",
        vec![Value::Str(url.into()), Value::Str(key.into())],
        policy_with_net(),
    );
    assert_eq!(v, Value::Variant { name: "None".into(), args: vec![] });
}
