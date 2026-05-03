# Agent-sprint orchestrator: a deliberately small pipeline that
# shows the effect-typed security perimeter end-to-end.
#
#   fetch_task  ──► invoke_agent  ──► verify  ──► persist
#       [io]            [proc]        (pure)    [kv, fs_write, log]
#
# The `sprint` fn's signature lists the *union* of every stage's
# effects. The host's `--allow-effects` set must cover that union
# at the entry point, and `--allow-fs-read` / `--allow-fs-write` /
# `--allow-proc` then narrow each individual capability.
#
# Read the README for the threat model and run instructions.

import "./types"               as t
import "./stages/fetch_task"   as fetch
import "./stages/invoke_agent" as agent
import "./stages/verify"       as verify
import "./stages/persist"      as persist
import "std.log"               as log
import "std.str"               as str

fn sprint(
  task_path :: Str,
  agent_cmd :: Str,
  db_path   :: Str,
) -> [io, proc, kv, fs_write, log] Result[Str, Str] {
  log.info(str.concat("sprint starting: ", task_path))
  match fetch.run(task_path) {
    Ok(task) => match agent.run(agent_cmd, task) {
      Ok(output) => {
        let verdict := verify.run(task, output)
        persist.run(db_path, task, output, verdict)
      },
      Err(e) => {
        log.error(str.concat("agent failed: ", e))
        Err(e)
      },
    },
    Err(e) => {
      log.error(str.concat("fetch failed: ", e))
      Err(e)
    },
  }
}
