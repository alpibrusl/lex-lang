# lex-jobs example — welcome-email producer + worker
#
# Demonstrates the end-to-end loop:
#   1. Enqueue a "welcome_email" job with a JSON payload.
#   2. Run a worker that dispatches on handler name.
#   3. Worker prints, marks Done. Producer/worker share a single
#      sqlite DB file for the demo.
#
# Run the producer (one-shot, queues a job then exits):
#   lex run --allow-effects io,sql,time,fs_write \
#     examples/welcome_email.lex produce
#
# Run the worker (blocks forever, polling every 500ms):
#   lex run --allow-effects io,sql,time,fs_write,crypto,random,fs_read,net,concurrent \
#     examples/welcome_email.lex consume

import "std.sql" as sql

import "std.io" as io

import "std.time" as time

import "std.str" as str

import "std.int" as int

import "../src/jobs" as jobs

# ---- Shared DB setup --------------------------------------------
fn open_demo_db() -> [sql, fs_write] Result[Db, Str] {
  match sql.open("/tmp/lex_jobs_demo.sqlite") {
    Err(e) => Err(e.message),
    Ok(db) => match jobs.init_schema(db) {
      Err(m) => Err(m),
      Ok(_) => Ok(db),
    },
  }
}

# ---- Producer ----------------------------------------------------
fn produce() -> [io, sql, time, fs_write] Unit {
  match open_demo_db() {
    Err(m) => io.print(str.concat("open failed: ", m)),
    Ok(db) => match jobs.enqueue(db, "emails", "welcome_email", "{\"user_id\":42,\"email\":\"alice@example.com\"}") {
      Err(m) => io.print(str.concat("enqueue failed: ", m)),
      Ok(id) => io.print(str.concat("enqueued job ", int.to_str(id))),
    },
  }
}

# ---- Worker ------------------------------------------------------
# Dispatch handler — pattern-matches on the handler-name string.
# Real apps would parse `payload` (JSON) with lex-schema.
fn dispatch(handler :: Str, payload :: Str) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] jobs.WorkOutcome {
  if handler == "welcome_email" {
    let __lex_log := io.print(str.concat("sending welcome email; payload: ", payload))
    Done
  } else {
    Fail(str.concat("unknown handler: ", handler))
  }
}

fn consume() -> [io, sql, time, fs_write, crypto, random, fs_read, net, concurrent] Unit {
  match open_demo_db() {
    Err(m) => io.print(str.concat("open failed: ", m)),
    Ok(db) => {
      let __lex_log := io.print("worker started (Ctrl-C to stop)")
      match jobs.work_forever(db, "emails", 500, dispatch) {
        Err(m) => io.print(str.concat("worker exited with error: ", m)),
        Ok(_) => (),
      }
    },
  }
}

