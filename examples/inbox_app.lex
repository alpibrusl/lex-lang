# An inbox processor that ROUTES events by classification and lets the
# *function signatures* carry the security policy. Reader sees at a
# glance:
#
#   classify         — pure (no effects)
#   handle_important — [net]      can post to webhooks, can't touch fs
#   handle_spam      — [io]       can write logs, can't make net calls
#   handle_followup  — [time, io] can read clock + write reminders
#
# Adding a network call to handle_spam is a TYPE ERROR, not a code-review
# bug. Granting [io] to the whole process and trusting that handle_spam
# won't do something else is exactly what RestrictedPython / sandboxes
# can't promise.
#
# Run:
#   lex run --allow-effects io,net,time \
#           --allow-fs-write /tmp \
#           examples/inbox_app.lex main
#
# Send an event:
#   curl -X POST http://127.0.0.1:8200/hook \
#     -H 'content-type: application/json' \
#     -d '{"from":"a@b","subject":"URGENT: down","body":"prod is on fire"}'

import "std.net"  as net
import "std.io"   as io
import "std.str"  as str
import "std.int"  as int
import "std.json" as json
import "std.time" as time

type Email   = { body :: Str, from :: Str, subject :: Str }
type Request = { body :: Str, method :: Str, path :: Str, query :: Str }
type Response = { body :: Str, status :: Int }

# ---- classification (pure) -----------------------------------------
# No effects. The reader can tell at a glance that classify can't write
# files, can't call APIs, can't read the clock — the type system says so.

type Action = Important | Spam | FollowUp | Other

fn classify(e :: Email) -> Action {
  match str.contains(str.to_lower(e.subject), "urgent") {
    true  => Important,
    false => match str.contains(str.to_lower(e.subject), "follow up") {
      true  => FollowUp,
      false => match str.contains(str.to_lower(e.subject), "win a prize") {
        true  => Spam,
        false => Other,
      },
    },
  }
}

# ---- handlers (each with its own narrow effect set) ----------------

# handle_important can hit the network. It cannot write files. If you
# accidentally added `io.write(...)` here, the type checker would
# reject the whole program.
fn handle_important(e :: Email) -> [net] Str {
  let url := "http://example.com/slack-mock"
  match net.post(url, str.concat("alert: ", e.subject)) {
    Ok(_)  => "posted",
    Err(m) => str.concat("post-failed: ", m),
  }
}

# handle_spam can write to a log file. It cannot make network calls.
fn handle_spam(e :: Email) -> [io] Str {
  let line := str.concat(str.concat(e.from, " | "), e.subject)
  match io.write("/tmp/lex_inbox_spam.log", line) {
    Ok(_)  => "logged",
    Err(m) => str.concat("log-failed: ", m),
  }
}

# handle_followup needs *both* the clock (to stamp the reminder) and
# the filesystem (to persist it). Any other capability is rejected.
fn handle_followup(e :: Email) -> [time, io] Str {
  let now  := time.now()
  let stamp := int.to_str(now)
  let entry := str.concat(stamp, str.concat(" | follow-up: ", e.subject))
  match io.write("/tmp/lex_inbox_followups.log", entry) {
    Ok(_)  => str.concat("scheduled at ", stamp),
    Err(m) => str.concat("schedule-failed: ", m),
  }
}

# Catch-all. Pure: returns a string, touches nothing.
fn handle_other(e :: Email) -> Str {
  str.concat("ignored: ", e.subject)
}

# ---- routing -------------------------------------------------------
# Route's effect set is the *union* of the handlers it can call.
# `[net, time, io]` here means: any code path through route may
# touch any of those three. If a future contributor adds a new
# variant whose handler uses [proc], the inferred set on route
# changes and the type checker forces the signature update.

fn route(e :: Email) -> [net, time, io] Str {
  match classify(e) {
    Important => handle_important(e),
    Spam      => handle_spam(e),
    FollowUp  => handle_followup(e),
    Other     => handle_other(e),
  }
}

# ---- HTTP glue -----------------------------------------------------

fn parse_email(body :: Str) -> Email {
  match json.parse(body) {
    Ok(e)  => e,
    Err(_) => { body: "", from: "", subject: "" },
  }
}

fn handle(req :: Request) -> [net, time, io] Response {
  match req.method {
    "POST" => match req.path {
      "/hook" => {
        let email := parse_email(req.body)
        let result := route(email)
        let resp := str.concat("{\"result\":\"", str.concat(result, "\"}"))
        { body: resp, status: 200 }
      },
      _ => { body: "{\"error\":\"not found\"}", status: 404 },
    },
    "GET" => match req.path {
      "/" => { body: "{\"endpoints\":[\"POST /hook\"]}", status: 200 },
      _   => { body: "{\"error\":\"not found\"}", status: 404 },
    },
    _ => { body: "{\"error\":\"method not allowed\"}", status: 405 },
  }
}

fn main() -> [net, time, io] Nil {
  net.serve(8200, "handle")
}
