# lex-jobs — smoke tests.
#
# Uses an in-memory sqlite DB so each test gets a fresh state and
# no fs-write policy is required. Real production uses Postgres
# (see README); these tests exercise the SQL surface that's common
# between the two.

import "std.sql" as sql

import "std.str" as str

import "std.list" as list

import "std.int" as int

import "../src/jobs" as jobs

# ---- Fixtures ----------------------------------------------------
# Fresh in-memory DB with the lex_jobs table created.
fn fresh_db() -> [sql, fs_write] Result[Db, Str] {
  match sql.open(":memory:") {
    Err(e) => Err(e.message),
    Ok(db) => match jobs.init_schema(db) {
      Err(m) => Err(m),
      Ok(_) => Ok(db),
    },
  }
}

# Always-Done dispatch for happy-path tests.
fn always_done(_h :: Str, _p :: Str) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] jobs.WorkOutcome {
  Done
}

# Always-Fail dispatch for failure-path tests.
fn always_fail(_h :: Str, _p :: Str) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] jobs.WorkOutcome {
  Fail("nope")
}

# Always-Retry dispatch — drives the retry-bookkeeping path.
fn always_retry(_h :: Str, _p :: Str) -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] jobs.WorkOutcome {
  Retry("transient")
}

# ---- Tests -------------------------------------------------------
fn init_schema_is_idempotent() -> [sql, time, fs_write] Result[Unit, Str] {
  match sql.open(":memory:") {
    Err(e) => Err(e.message),
    Ok(db) => match jobs.init_schema(db) {
      Err(m) => Err(str.concat("first init: ", m)),
      Ok(_) => match jobs.init_schema(db) {
        Err(m) => Err(str.concat("second init: ", m)),
        Ok(_) => Ok(()),
      },
    },
  }
}

fn enqueue_returns_increasing_ids() -> [sql, time, fs_write] Result[Unit, Str] {
  match fresh_db() {
    Err(m) => Err(m),
    Ok(db) => match jobs.enqueue(db, "q", "h", "{}") {
      Err(m) => Err(str.concat("first enqueue: ", m)),
      Ok(id1) => match jobs.enqueue(db, "q", "h", "{}") {
        Err(m) => Err(str.concat("second enqueue: ", m)),
        Ok(id2) => if id2 > id1 {
          Ok(())
        } else {
          Err(str.concat("ids not increasing: ", str.concat(int.to_str(id1), str.concat(" -> ", int.to_str(id2)))))
        },
      },
    },
  }
}

fn count_pending_reflects_enqueue() -> [sql, time, fs_write] Result[Unit, Str] {
  match fresh_db() {
    Err(m) => Err(m),
    Ok(db) => match jobs.enqueue(db, "q", "h", "{}") {
      Err(m) => Err(m),
      Ok(_) => match jobs.enqueue(db, "q", "h", "{}") {
        Err(m) => Err(m),
        Ok(_) => match jobs.count_pending(db, "q") {
          Err(m) => Err(m),
          Ok(n) => if n == 2 {
            Ok(())
          } else {
            Err(str.concat("expected 2, got ", int.to_str(n)))
          },
        },
      },
    },
  }
}

fn work_one_done_clears_the_queue() -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Result[Unit, Str] {
  match fresh_db() {
    Err(m) => Err(m),
    Ok(db) => match jobs.enqueue(db, "q", "h", "{}") {
      Err(m) => Err(m),
      Ok(_) => match jobs.work_one(db, "q", always_done) {
        Err(m) => Err(str.concat("work_one: ", m)),
        Ok(None) => Err("expected one job processed, got none"),
        Ok(Some(_)) => match jobs.count_pending(db, "q") {
          Err(m) => Err(m),
          Ok(n) => if n == 0 {
            Ok(())
          } else {
            Err(str.concat("expected 0 pending, got ", int.to_str(n)))
          },
        },
      },
    },
  }
}

fn work_one_on_empty_queue_returns_none() -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Result[Unit, Str] {
  match fresh_db() {
    Err(m) => Err(m),
    Ok(db) => match jobs.work_one(db, "q", always_done) {
      Err(m) => Err(m),
      Ok(None) => Ok(()),
      Ok(Some(_)) => Err("expected None on empty queue, got Some"),
    },
  }
}

fn fail_outcome_marks_job_failed_not_pending() -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Result[Unit, Str] {
  match fresh_db() {
    Err(m) => Err(m),
    Ok(db) => match jobs.enqueue(db, "q", "h", "{}") {
      Err(m) => Err(m),
      Ok(_) => match jobs.work_one(db, "q", always_fail) {
        Err(m) => Err(m),
        Ok(_) => match jobs.count_pending(db, "q") {
          Err(m) => Err(m),
          Ok(n) => if n == 0 {
            Ok(())
          } else {
            Err(str.concat("Fail should not leave pending; got ", int.to_str(n)))
          },
        },
      },
    },
  }
}

fn retry_outcome_under_max_attempts_returns_to_pending() -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Result[Unit, Str] {
  match fresh_db() {
    Err(m) => Err(m),
    Ok(db) => match jobs.enqueue(db, "q", "h", "{}") {
      Err(m) => Err(m),
      Ok(_) => match jobs.work_one(db, "q", always_retry) {
        Err(m) => Err(m),
        Ok(_) => match jobs.count_pending(db, "q") {
          Err(m) => Err(m),
          Ok(n) => if n == 1 {
            Ok(())
          } else {
            Err(str.concat("retry under cap should re-pend; got ", int.to_str(n)))
          },
        },
      },
    },
  }
}

fn delayed_job_not_immediately_eligible() -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Result[Unit, Str] {
  let opts := { delay_seconds: 3600, max_attempts: 3 }
  match fresh_db() {
    Err(m) => Err(m),
    Ok(db) => match jobs.enqueue_with(db, "q", "h", "{}", opts) {
      Err(m) => Err(m),
      Ok(_) => match jobs.work_one(db, "q", always_done) {
        Err(m) => Err(m),
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err("delayed job ran early"),
      },
    },
  }
}

# ---- Suite -------------------------------------------------------
fn suite() -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] List[Result[Unit, Str]] {
  [init_schema_is_idempotent(), enqueue_returns_increasing_ids(), count_pending_reflects_enqueue(), work_one_done_clears_the_queue(), work_one_on_empty_queue_returns_none(), fail_outcome_marks_job_failed_not_pending(), retry_outcome_under_max_attempts_returns_to_pending(), delayed_job_not_immediately_eligible()]
}

fn run_all() -> [io, time, crypto, random, sql, fs_read, fs_write, net, concurrent] Unit {
  let failures := list.fold(suite(), 0, fn (n :: Int, r :: Result[Unit, Str]) -> Int {
    match r {
      Ok(_) => n,
      Err(_) => n + 1,
    }
  })
  if failures == 0 {
    ()
  } else {
    let __lex_discard_1 := 1 / 0
    ()
  }
}

