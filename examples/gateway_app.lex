# A personal automation gateway. Multiple routes, each with its own
# narrow effect signature. The point: when you read the source, every
# route tells you exactly what it can do *to your machine* before
# you've read a single line of its body.
#
#   GET  /                  pure       help text
#   GET  /now               [time]     unix timestamp
#   POST /classify          pure       keyword classifier
#   POST /summarize         pure       first N chars
#   GET  /weather/:city     [net]      external weather call
#
# (A planned /digest route — read a bookmarks file, fetch each URL —
# is deferred. It needs `list.map` over an effectful closure, which
# requires effect polymorphism in stdlib higher-order signatures.
# Spec §7.3 designs this; the type system doesn't yet implement it.
# Tracked as follow-up.)
#
# Compare to a Python flask app where every route gets the union of
# capabilities (the whole process). Here, /classify *physically cannot*
# touch the network; /summarize *physically cannot* read the clock; and
# any drift gets caught at type-check, not in code review.
#
# Run:
#   lex run --allow-effects io,net,time \
#           --allow-fs-read /tmp \
#           --allow-net-host wttr.in,httpbin.org \
#           examples/gateway_app.lex main
#
# Try:
#   curl http://127.0.0.1:8210/now
#   curl -X POST -d "long text..." http://127.0.0.1:8210/summarize
#   curl http://127.0.0.1:8210/weather/Paris

import "std.net"  as net
import "std.io"   as io
import "std.str"  as str
import "std.int"  as int
import "std.list" as list
import "std.time" as time

type Request  = { body :: Str, method :: Str, path :: Str, query :: Str }
type Response = { body :: Str, status :: Int }

# Helper so match arms can return bare error responses without
# tripping the alias-vs-structural-record unifier in handle's
# branches. err(...) returns the Response alias; route_X(...)
# returns the alias; they unify nominally.
fn err(msg :: Str, code :: Int) -> Response {
  { body: msg, status: code }
}

fn ok(msg :: Str) -> Response {
  { body: msg, status: 200 }
}

# ---- pure routes ---------------------------------------------------

fn route_help() -> Response {
  let body := "{\"endpoints\":[\"GET /now\",\"POST /classify\",\"POST /summarize\",\"GET /weather/:city\"]}"
  { body: body, status: 200 }
}

# Truncate to the first 80 chars. In production this would call an LLM
# (and the signature would gain `[llm]` accordingly). Today: pure.
fn route_summarize(body :: Str) -> Response {
  let head := match str.len(body) > 80 {
    true  => str.concat(str.slice(body, 0, 80), "…"),
    false => body,
  }
  let resp := str.concat("{\"summary\":\"", str.concat(head, "\"}"))
  { body: resp, status: 200 }
}

# Keyword classifier. Pure — touches nothing, deterministic.
fn route_classify(body :: Str) -> Response {
  let low := str.to_lower(body)
  let label := match str.contains(low, "urgent") {
    true => "important",
    false => match str.contains(low, "win a prize") {
      true => "spam",
      false => match str.contains(low, "follow up") {
        true => "followup",
        false => "other",
      },
    },
  }
  let resp := str.concat("{\"label\":\"", str.concat(label, "\"}"))
  { body: resp, status: 200 }
}

# ---- [time] route --------------------------------------------------

fn route_now() -> [time] Response {
  let now := time.now()
  let resp := str.concat("{\"now\":", str.concat(int.to_str(now), "}"))
  { body: resp, status: 200 }
}

# ---- [net] route ---------------------------------------------------

# Calls wttr.in's plain-text weather endpoint. The host's
# --allow-net-host flag pins which hosts are reachable; everything
# else is rejected at the runtime gate before the request is sent.
fn route_weather(city :: Str) -> [net] Response {
  let url := str.concat("http://wttr.in/", str.concat(city, "?format=3"))
  match net.get(url) {
    Ok(s)  => { body: str.concat("{\"weather\":\"", str.concat(s, "\"}")), status: 200 },
    Err(e) => { body: str.concat("{\"error\":\"", str.concat(e, "\"}")), status: 502 },
  }
}

# ---- dispatcher ----------------------------------------------------

# Top-level handler's effect set is the union of every route. If you
# add a new route that uses an effect not already declared here, the
# checker forces you to update this signature — the policy stays in
# sync with the code, mechanically.
fn handle(req :: Request) -> [net, time] Response {
  match req.method {
    "GET" => match req.path {
      "/"     => route_help(),
      "/now"  => route_now(),
      _ => match str.strip_prefix(req.path, "/weather/") {
        Some(city) => route_weather(city),
        None       => err("{\"error\":\"not found\"}", 404),
      },
    },
    "POST" => match req.path {
      "/classify"  => route_classify(req.body),
      "/summarize" => route_summarize(req.body),
      _ => err("{\"error\":\"not found\"}", 404),
    },
    _ => err("{\"error\":\"method not allowed\"}", 405),
  }
}

fn main() -> [net, time] Nil {
  net.serve(8210, "handle")
}
