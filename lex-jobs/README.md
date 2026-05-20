# lex-jobs — durable background job queue

> **Status: incubator (v0.1).** Lives in `lex-lang/lex-jobs/` while
> the API surface firms up; the plan is to extract to
> `alpibrusl/lex-jobs` as a standalone package once the design has
> seen real use. Tracked in lex-lang#489.

A SQL-backed work queue for Lex apps. Producers enqueue jobs from
HTTP handlers (or anywhere with a `[sql]` effect); workers pull,
dispatch, and ack them.

Closes the gap called out by the framework-stack comparison: lex
already had fire-and-forget concurrency via `conc.spawn`, but no
**durable** queue — the Celery / BullMQ / Sidekiq layer that real
production apps need for email sending, webhook delivery, retries
across restarts, scheduled work, etc.

## Quick start

```lex
import "std.sql" as sql
import "lex-jobs/src/jobs" as jobs   # imports change once extracted

# 1. Bootstrap the schema at app startup (idempotent).
let db = match sql.open(db_url) { Ok(d) => d, Err(_) => panic("...") }
let _  = jobs.init_schema(db)

# 2. Enqueue from an HTTP handler.
jobs.enqueue(db, "emails", "welcome_email", json_payload)

# 3. Run a worker (separate process or thread).
jobs.work_forever(db, "emails", 500, dispatch_fn)
```

A complete runnable example lives at `examples/welcome_email.lex`.

## Design

* **One SQL table**, `lex_jobs`. Schema fits SQLite + Postgres
  unchanged: see `init_schema` in `src/jobs.lex`.
* **Producers** call `enqueue(db, queue, handler, payload)` —
  returns the new row id. `enqueue_with(..., opts)` accepts a
  `JobOpts` record with `delay_seconds` and `max_attempts`.
* **Dispatch** is a single user-supplied function:
  `(handler_name :: Str, payload :: Str) -> [...] WorkOutcome`.
  The worker pattern-matches on `handler_name` to route to the
  right business logic. Payloads are opaque strings — pair with
  `lex-schema` for typed encode/decode.
* **Workers** call `work_one(db, queue, dispatch)` (one job, returns
  immediately) or `work_forever(db, queue, sleep_ms, dispatch)`
  (blocking poll loop with empty-queue backoff).
* **Outcomes**: `Done` → ack and remove from queue;
  `Retry(reason)` → return to `pending` for re-dispatch up to
  `max_attempts`; `Fail(reason)` → terminal failure, status moves
  to `failed`.

## Production use — Postgres

```sh
# Start a Postgres + provision database
psql -c "CREATE DATABASE lex_jobs_demo"
```

```lex
let db = sql.open("postgres://localhost/lex_jobs_demo")
jobs.init_schema(db)   # works on Postgres unchanged
```

The schema's `INTEGER PRIMARY KEY AUTOINCREMENT` is accepted by
Postgres (where it behaves like `BIGSERIAL`). For high-volume
production deployments you may prefer a hand-tuned schema with
`BIGSERIAL`, `TIMESTAMPTZ`, and a partial index — the v1 schema is
a portable middle ground, not the absolute fastest shape.

## Limitations (v0.1)

These are deliberate scope cuts, each tracked as a follow-up on
lex-lang#489:

* **Multi-worker race window.** The dequeue uses
  `UPDATE ... WHERE id = (SELECT ... LIMIT 1) RETURNING ...`. On
  Postgres this is racy across concurrent workers (two can claim
  the same id before either `UPDATE` lands). Multi-worker safety
  needs `SELECT ... FOR UPDATE SKIP LOCKED` on Postgres; v2.
* **No cron / recurring jobs.** Use `delay_seconds` for one-shot
  scheduled work. Recurring schedules need a separate scheduler
  loop or a `recurring_jobs` table; v2.
* **No structured backoff.** A `Retry` outcome puts the job back to
  `pending` immediately. Production usage often wants
  exponential / jittered delays before re-dispatch; v2.
* **No dead-letter queue.** Jobs that exhaust `max_attempts` land
  in `status='failed'` and stay there. A DLQ that lets you replay
  or inspect failures with structured metadata is v2.
* **No observability hooks.** `count_pending(db, queue)` is the
  only built-in metric. Production needs structured per-job
  events (claimed, succeeded, failed) routed to logs/metrics; v2.
* **No Redis backend.** Postgres-first by design; Redis as a
  drop-in backend is plausible but not in v1.

## Why pure Lex (no Rust)

The whole point of the lex effect system is that ordinary
application code — HTTP handlers, workers, schedulers — should be
expressible in Lex with the right effect signatures. `sql.exec` /
`sql.query` already give us everything a durable queue needs.
Pushing the implementation into a Rust crate would buy nothing
except an installation step.

## Running the tests

```sh
cd lex-jobs/
lex check  src/jobs.lex
lex check  tests/test_jobs.lex
lex test --allow-effects io,time,crypto,random,sql,fs_read,fs_write,net,concurrent tests/
```

Tests run against in-memory SQLite. Postgres integration tests
need a running Postgres and live in a follow-up.

## Running the example

```sh
cd lex-jobs/

# producer (one-shot)
lex run --allow-effects io,sql,time,fs_write,crypto,random,fs_read,net,concurrent \
        --allow-fs-write /tmp \
        examples/welcome_email.lex produce

# worker (blocks forever; Ctrl-C to stop)
lex run --allow-effects io,sql,time,fs_write,crypto,random,fs_read,net,concurrent \
        --allow-fs-write /tmp \
        examples/welcome_email.lex consume
```

## Extraction plan

When the API has been used in anger for a release cycle or two and
the v2 items above are sketched out, this directory moves to
`alpibrusl/lex-jobs` as a standalone Lex package. Downstream apps
will pin it via `lex.toml`:

```toml
[dependencies]
lex-jobs = { git = "https://github.com/alpibrusl/lex-jobs" }
```

Until then, downstream apps using lex-jobs live in the same repo
tree as their lex-lang checkout.
