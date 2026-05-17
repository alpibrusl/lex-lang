# lex-jobs — durable background job queue (v0.1)
#
# A SQL-backed work queue. Producers `enqueue` jobs; one or more
# workers `work_forever` to pull, dispatch, and ack them. Designed
# for Postgres in production, with SQLite as a self-contained
# backend for local development and tests.
#
# Public surface:
#   - init_schema(db)                          create the lex_jobs table
#   - enqueue(db, queue, handler, payload)     enqueue immediately
#   - enqueue_with(db, ..., opts)              with delay + max_attempts
#   - work_one(db, queue, dispatch)            process one job (if any)
#   - work_forever(db, queue, sleep_ms, ...)   loop forever
#   - count_pending(db, queue)                 observability helper
#
# v1 limitations (see README "Limitations" section):
#   - Single-worker safe; multi-worker on PostgreSQL has a small
#     race window. Multi-worker safety via SELECT FOR UPDATE SKIP
#     LOCKED is tracked as a follow-up.
#   - No cron / recurring jobs (use `delay_seconds` for one-shot delay).
#   - No dead-letter queue — exhausted-retry jobs land in status='failed'.
#   - No structured backoff — failed jobs are re-eligible immediately
#     up to max_attempts.

import "std.sql" as sql

import "std.time" as time

import "std.int" as int

import "std.list" as list

# ---- Public types ------------------------------------------------
# Options at enqueue time.
type JobOpts = { delay_seconds :: Int, max_attempts :: Int }

# Outcome of a worker dispatch — drives ack / retry / fail.
type WorkOutcome = Done | Retry(Str) | Fail(Str)

type JobRow = { id :: Int, queue :: Str, handler :: Str, payload :: Str, attempts :: Int, max_attempts :: Int }

fn default_opts() -> JobOpts {
  { delay_seconds: 0, max_attempts: 3 }
}

# ---- Dispatch function shape -------------------------------------
# Worker handlers run under the wide effect row so they can do
# anything an HTTP handler can — match `lex-web`'s convention.
#
# This type alias is documentation only; lex 0.9.x rejects calling
# through a function-type alias, so signatures below inline the
# function type. See lex-web/src/lifespan.lex for prior art.
#
#   type DispatchFn = (Str, Str) -> [io, time, crypto, random, sql,
#                                    fs_read, fs_write, net, concurrent] WorkOutcome
# ---- Schema bootstrap --------------------------------------------
# Idempotent — safe to call on every app boot. Schema uses the
# common subset of SQLite + PostgreSQL DDL that both accept.
#
# Production Postgres deployments may prefer a hand-tuned schema
# (BIGSERIAL, partial index, TIMESTAMPTZ); see README.
fn init_schema(db :: Db) -> [sql] Result[Unit, Str] {
  let create_table := "CREATE TABLE IF NOT EXISTS lex_jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, queue TEXT NOT NULL, handler TEXT NOT NULL, payload TEXT NOT NULL, scheduled_at INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, max_attempts INTEGER NOT NULL DEFAULT 3, status TEXT NOT NULL DEFAULT 'pending', last_error TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)"
  let create_index := "CREATE INDEX IF NOT EXISTS lex_jobs_dispatch ON lex_jobs (queue, status, scheduled_at)"
  match sql.exec(db, create_table, []) {
    Err(e) => Err(e.message),
    Ok(_) => match sql.exec(db, create_index, []) {
      Err(e) => Err(e.message),
      Ok(_) => Ok(()),
    },
  }
}

# ---- Producer ----------------------------------------------------
fn enqueue(db :: Db, queue :: Str, handler :: Str, payload :: Str) -> [sql, time] Result[Int, Str] {
  enqueue_with(db, queue, handler, payload, default_opts())
}

fn enqueue_with(db :: Db, queue :: Str, handler :: Str, payload :: Str, opts :: JobOpts) -> [sql, time] Result[Int, Str] {
  let now := time.now_ms() / 1000
  let scheduled := now + opts.delay_seconds
  let row_result :: Result[List[{ id :: Int }], SqlError] := sql.query(db, "INSERT INTO lex_jobs (queue, handler, payload, scheduled_at, max_attempts, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id", [PStr(queue), PStr(handler), PStr(payload), PInt(scheduled), PInt(opts.max_attempts), PInt(now), PInt(now)])
  match row_result {
    Err(e) => Err(e.message),
    Ok(rows) => match list.head(rows) {
      None => Err("INSERT...RETURNING returned no rows"),
      Some(r) => Ok(r.id),
    },
  }
}

# ---- Worker ------------------------------------------------------
# `work_one` pulls at most one ready job, hands it to `dispatch`,
# acks/retries/fails based on the outcome, and returns:
#   - Ok(Some(row)) — a job ran (check WorkOutcome via the row's
#     updated row in db; this fn returns the *pre-dispatch* row)
#   - Ok(None)      — no ready job
#   - Err(msg)      — infrastructure failure (DB, claim race lost)
fn work_one(db :: Db, queue :: Str, dispatch :: (Str, Str) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] WorkOutcome) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Result[Option[JobRow], Str] {
  match try_claim(db, queue) {
    Err(msg) => Err(msg),
    Ok(None) => Ok(None),
    Ok(Some(job)) => match dispatch(job.handler, job.payload) {
      Done => match ack(db, job.id) {
        Err(m) => Err(m),
        Ok(_) => Ok(Some(job)),
      },
      Retry(why) => match retry_or_fail(db, job, why) {
        Err(m) => Err(m),
        Ok(_) => Ok(Some(job)),
      },
      Fail(why) => match fail(db, job.id, why) {
        Err(m) => Err(m),
        Ok(_) => Ok(Some(job)),
      },
    },
  }
}

# Block forever, processing jobs as they arrive. When the queue is
# empty, sleeps `sleep_ms` between polls. Errors are returned (the
# caller decides whether to crash, log + retry, etc.).
fn work_forever(db :: Db, queue :: Str, sleep_ms :: Int, dispatch :: (Str, Str) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] WorkOutcome) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Result[Unit, Str] {
  match work_one(db, queue, dispatch) {
    Err(m) => Err(m),
    Ok(Some(_)) => work_forever(db, queue, sleep_ms, dispatch),
    Ok(None) => {
      let __lex_sleep := time.sleep_ms(sleep_ms)
      work_forever(db, queue, sleep_ms, dispatch)
    },
  }
}

# ---- Observability -----------------------------------------------
fn count_pending(db :: Db, queue :: Str) -> [sql] Result[Int, Str] {
  let row_result :: Result[List[{ n :: Int }], SqlError] := sql.query(db, "SELECT COUNT(*) AS n FROM lex_jobs WHERE queue = ? AND status = 'pending'", [PStr(queue)])
  match row_result {
    Err(e) => Err(e.message),
    Ok(rows) => match list.head(rows) {
      None => Ok(0),
      Some(r) => Ok(r.n),
    },
  }
}

# ---- Internals: claim + ack / retry / fail -----------------------
# Atomically claim the next ready job for this queue. The SELECT-
# subquery + outer UPDATE pattern is a single statement; the
# `lex_jobs_dispatch` index makes the inner SELECT cheap.
#
# Multi-worker race note: on Postgres, two concurrent workers can
# both see the same id in the subquery before either UPDATE lands.
# Both UPDATEs then succeed and both workers think they own the job.
# Fixed by wrapping the subquery in SELECT...FOR UPDATE SKIP LOCKED
# (Postgres-only); tracked as a v2 follow-up. v1 is safe with a
# single worker per queue.
fn try_claim(db :: Db, queue :: Str) -> [sql, time] Result[Option[JobRow], Str] {
  let now := time.now_ms() / 1000
  let row_result :: Result[List[JobRow], SqlError] := sql.query(db, "UPDATE lex_jobs SET status = 'running', attempts = attempts + 1, updated_at = ? WHERE id = (SELECT id FROM lex_jobs WHERE queue = ? AND status = 'pending' AND scheduled_at <= ? ORDER BY scheduled_at, id LIMIT 1) RETURNING id, queue, handler, payload, attempts, max_attempts", [PInt(now), PStr(queue), PInt(now)])
  match row_result {
    Err(e) => Err(e.message),
    Ok(rows) => match list.head(rows) {
      None => Ok(None),
      Some(r) => Ok(Some(r)),
    },
  }
}

fn ack(db :: Db, id :: Int) -> [sql, time] Result[Unit, Str] {
  let now := time.now_ms() / 1000
  match sql.exec(db, "UPDATE lex_jobs SET status = 'done', updated_at = ? WHERE id = ?", [PInt(now), PInt(id)]) {
    Err(e) => Err(e.message),
    Ok(_) => Ok(()),
  }
}

# Retry on transient failure: if the job has remaining attempts,
# return it to status='pending'; otherwise mark it failed.
fn retry_or_fail(db :: Db, job :: JobRow, why :: Str) -> [sql, time] Result[Unit, Str] {
  if job.attempts >= job.max_attempts {
    fail(db, job.id, why)
  } else {
    requeue(db, job.id, why)
  }
}

fn requeue(db :: Db, id :: Int, why :: Str) -> [sql, time] Result[Unit, Str] {
  let now := time.now_ms() / 1000
  match sql.exec(db, "UPDATE lex_jobs SET status = 'pending', last_error = ?, updated_at = ? WHERE id = ?", [PStr(why), PInt(now), PInt(id)]) {
    Err(e) => Err(e.message),
    Ok(_) => Ok(()),
  }
}

fn fail(db :: Db, id :: Int, why :: Str) -> [sql, time] Result[Unit, Str] {
  let now := time.now_ms() / 1000
  match sql.exec(db, "UPDATE lex_jobs SET status = 'failed', last_error = ?, updated_at = ? WHERE id = ?", [PStr(why), PInt(now), PInt(id)]) {
    Err(e) => Err(e.message),
    Ok(_) => Ok(()),
  }
}

