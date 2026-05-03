//! Integration tests for `std.datetime`. Closes #101.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

fn policy_with_time() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["time".to_string()].into_iter().collect::<BTreeSet<_>>();
    p
}

fn run(src: &str, fn_name: &str, args: Vec<Value>, policy: Policy) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn s(v: Value) -> String {
    match v {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.datetime" as datetime

fn parse_then_format(iso :: Str) -> Str {
  match datetime.parse_iso(iso) {
    Ok(t)  => datetime.format_iso(t),
    Err(_) => "<bad iso>",
  }
}

fn parse_iso_ok(iso :: Str) -> Bool {
  match datetime.parse_iso(iso) {
    Ok(_)  => true,
    Err(_) => false,
  }
}

fn add_seconds(iso :: Str, secs :: Float) -> Str {
  match datetime.parse_iso(iso) {
    Ok(t) => {
      let d := datetime.duration_seconds(secs)
      datetime.format_iso(datetime.add(t, d))
    },
    Err(_) => "<bad iso>",
  }
}

fn diff_is_45_minutes(start :: Str, end :: Str) -> Bool {
  match datetime.parse_iso(start) {
    Ok(a) => match datetime.parse_iso(end) {
      Ok(b) => datetime.diff(b, a) == datetime.duration_minutes(45),
      Err(_) => false,
    },
    Err(_) => false,
  }
}

fn year_in_utc(iso :: Str) -> Int {
  match datetime.parse_iso(iso) {
    Ok(t) => match datetime.to_components(t, Utc) {
      Ok(c)  => c.year,
      Err(_) => 0 - 1,
    },
    Err(_) => 0 - 1,
  }
}

fn tz_offset_for(iso :: Str, tz :: Tz) -> Int {
  match datetime.parse_iso(iso) {
    Ok(t) => match datetime.to_components(t, tz) {
      Ok(c)  => c.tz_offset_minutes,
      Err(_) => 0 - 99999,
    },
    Err(_) => 0 - 99999,
  }
}

fn round_trip_components(iso :: Str) -> Str {
  match datetime.parse_iso(iso) {
    Ok(t) => match datetime.to_components(t, Utc) {
      Ok(c) => match datetime.from_components(c) {
        Ok(t2) => datetime.format_iso(t2),
        Err(_) => "<from_components fail>",
      },
      Err(_) => "<to_components fail>",
    },
    Err(_) => "<bad iso>",
  }
}

# Tag-only summary of what each Tz variant carries — exercises a
# match across every constructor.
fn tz_tag(tz :: Tz) -> Str {
  match tz {
    Utc       => "utc",
    Local     => "local",
    Offset(_) => "offset",
    Iana(_)   => "iana",
  }
}

# Surface the IANA name back out — exercises constructor-with-payload
# binding.
fn iana_name_or(tz :: Tz, fallback :: Str) -> Str {
  match tz {
    Iana(s) => s,
    _       => fallback,
  }
}

# Exercises the failure path: an unrecognized IANA name now produces
# the documented `Err` payload through the existing parse path.
fn tz_offset_or_err(iso :: Str, tz :: Tz) -> Str {
  match datetime.parse_iso(iso) {
    Ok(t) => match datetime.to_components(t, tz) {
      Ok(_)  => "<ok>",
      Err(e) => e,
    },
    Err(_) => "<bad iso>",
  }
}
"#;

#[test]
fn parse_iso_round_trip_preserves_instant() {
    let v = run(
        SRC,
        "parse_then_format",
        vec![Value::Str("2026-05-03T12:34:56+00:00".into())],
        Policy::pure(),
    );
    let out = s(v);
    // chrono uses the same RFC 3339 grammar; output should round-trip.
    assert!(
        out.starts_with("2026-05-03T12:34:56"),
        "expected round-trip, got: {out}"
    );
}

#[test]
fn parse_iso_rejects_garbage() {
    let v = run(
        SRC,
        "parse_iso_ok",
        vec![Value::Str("not a timestamp".into())],
        Policy::pure(),
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn add_seconds_advances_the_instant() {
    let v = run(
        SRC,
        "add_seconds",
        vec![
            Value::Str("2026-05-03T12:00:00+00:00".into()),
            Value::Float(90.5),
        ],
        Policy::pure(),
    );
    let out = s(v);
    // 12:00:00 + 90.5s = 12:01:30.5
    assert!(out.starts_with("2026-05-03T12:01:30"), "got: {out}");
}

#[test]
fn diff_equals_minute_duration() {
    let v = run(
        SRC,
        "diff_is_45_minutes",
        vec![
            Value::Str("2026-05-03T12:00:00+00:00".into()),
            Value::Str("2026-05-03T12:45:00+00:00".into()),
        ],
        Policy::pure(),
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn to_components_yields_year() {
    let v = run(
        SRC,
        "year_in_utc",
        vec![Value::Str("2026-05-03T12:00:00+00:00".into())],
        Policy::pure(),
    );
    assert_eq!(v, Value::Int(2026));
}

fn iana(name: &str) -> Value {
    Value::Variant {
        name: "Iana".into(),
        args: vec![Value::Str(name.into())],
    }
}

fn offset_minutes(m: i64) -> Value {
    Value::Variant {
        name: "Offset".into(),
        args: vec![Value::Int(m)],
    }
}

#[test]
fn iana_timezone_offset() {
    // 2026-01-15 (winter) New York is UTC-05:00 → -300 minutes.
    let v = run(
        SRC,
        "tz_offset_for",
        vec![
            Value::Str("2026-01-15T12:00:00+00:00".into()),
            iana("America/New_York"),
        ],
        Policy::pure(),
    );
    assert_eq!(v, Value::Int(-300));
}

#[test]
fn iana_timezone_offset_dst() {
    // 2026-07-15 (summer) New York is UTC-04:00 → -240 minutes.
    let v = run(
        SRC,
        "tz_offset_for",
        vec![
            Value::Str("2026-07-15T12:00:00+00:00".into()),
            iana("America/New_York"),
        ],
        Policy::pure(),
    );
    assert_eq!(v, Value::Int(-240));
}

#[test]
fn fixed_offset_components() {
    // +05:30 → 330 minutes east.
    let v = run(
        SRC,
        "tz_offset_for",
        vec![
            Value::Str("2026-05-03T12:00:00+00:00".into()),
            offset_minutes(330),
        ],
        Policy::pure(),
    );
    assert_eq!(v, Value::Int(330));
}

#[test]
fn round_trip_through_components() {
    let v = run(
        SRC,
        "round_trip_components",
        vec![Value::Str("2026-05-03T12:34:56+00:00".into())],
        Policy::pure(),
    );
    let out = s(v);
    assert!(out.starts_with("2026-05-03T12:34:56"), "got: {out}");
}

#[test]
fn datetime_now_returns_a_recent_instant() {
    // `datetime.now()` returns Instant; we ISO-format it and assert
    // the year is in a plausible window.
    let src = r#"
import "std.datetime" as datetime
fn now_iso() -> [time] Str { datetime.format_iso(datetime.now()) }
"#;
    let v = run(src, "now_iso", vec![], policy_with_time());
    let iso = s(v);
    // Year-prefix sanity check; covers 2020..2100.
    let year: i32 = iso.get(..4).and_then(|y| y.parse().ok())
        .unwrap_or_else(|| panic!("could not parse year from {iso}"));
    assert!((2020..2100).contains(&year), "now()'s year out of range: {year}");
}

#[test]
fn tz_match_returns_correct_tag_per_variant() {
    let utc   = Value::Variant { name: "Utc".into(),   args: vec![] };
    let local = Value::Variant { name: "Local".into(), args: vec![] };
    assert_eq!(s(run(SRC, "tz_tag", vec![utc],   Policy::pure())), "utc");
    assert_eq!(s(run(SRC, "tz_tag", vec![local], Policy::pure())), "local");
    assert_eq!(
        s(run(SRC, "tz_tag", vec![offset_minutes(60)], Policy::pure())),
        "offset",
    );
    assert_eq!(
        s(run(SRC, "tz_tag", vec![iana("UTC")], Policy::pure())),
        "iana",
    );
}

#[test]
fn iana_payload_is_destructurable() {
    let v = run(
        SRC,
        "iana_name_or",
        vec![iana("Europe/Paris"), Value::Str("none".into())],
        Policy::pure(),
    );
    assert_eq!(s(v), "Europe/Paris");
    // Non-Iana variant falls through to fallback.
    let v = run(
        SRC,
        "iana_name_or",
        vec![
            Value::Variant { name: "Utc".into(), args: vec![] },
            Value::Str("none".into()),
        ],
        Policy::pure(),
    );
    assert_eq!(s(v), "none");
}

#[test]
fn unknown_iana_name_returns_err_with_message() {
    // The Tz variant statically guarantees the *shape* of the input,
    // but an `Iana(...)` payload can still hold an invalid name.
    let v = run(
        SRC,
        "tz_offset_or_err",
        vec![
            Value::Str("2026-05-03T12:00:00+00:00".into()),
            iana("Not/Real_Place"),
        ],
        Policy::pure(),
    );
    let msg = s(v);
    assert!(
        msg.contains("Not/Real_Place") && msg.contains("unknown"),
        "expected unknown-timezone error, got: {msg}",
    );
}

#[test]
fn local_variant_returns_components_without_panicking() {
    // `Local` resolves against the host's current timezone; we don't
    // know what offset that produces in CI, so just assert it
    // returns a reasonable offset (within ±840 minutes — covers all
    // real timezones on Earth).
    let local = Value::Variant { name: "Local".into(), args: vec![] };
    let v = run(
        SRC,
        "tz_offset_for",
        vec![Value::Str("2026-05-03T12:00:00+00:00".into()), local],
        Policy::pure(),
    );
    match v {
        Value::Int(off) => assert!(
            (-840..=840).contains(&off),
            "Local offset out of plausible range: {off}",
        ),
        other => panic!("expected Int offset, got {other:?}"),
    }
}
