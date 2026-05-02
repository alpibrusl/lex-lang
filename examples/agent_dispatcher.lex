# An "agent dispatcher" that calls another CLI agent (claude-code,
# cursor-cli, gemini-cli, ...) over stdout. The signature declares
# `[proc]`, so the type checker rejects any body that tries to do
# anything but `proc.spawn`. The runtime gates on which binary
# basenames are spawnable via `--allow-proc`.
#
# This is the use case agents-orchestrating-agents need: a Lex
# program that dispatches to subprocess CLIs while keeping the
# attack surface visible at the type level.
#
# Run:
#   lex run --allow-effects proc --allow-proc echo \
#     examples/agent_dispatcher.lex run "echo" "hello world"
#
# Adversarial scenario:
#   The runtime *will* refuse to spawn a binary not in --allow-proc,
#   even when [proc] itself is granted. Try:
#     lex run --allow-effects proc --allow-proc echo \
#       examples/agent_dispatcher.lex run "rm" "-rf" "/"
#   → "proc.spawn: `rm` not in --allow-proc [\"echo\"]"
#
#   Exit codes:
#     0  — sub-binary returned 0 (success)
#     non-zero (returned in `exit_code`) — sub-binary failed
#     A `proc.spawn` not-allowed surfaces as Err(...), which we
#     return as the JSON output of `run`.

import "std.proc" as proc
import "std.list" as list
import "std.str" as str
import "std.int" as int

type Output = {
  ok :: Bool,
  exit_code :: Int,
  stdout :: Str,
  stderr :: Str,
}

# Dispatch to a sub-binary with a list of args; flatten the
# Result into a uniform Output record so a parent agent doesn't
# have to pattern-match on Result + Record.
fn run(cmd :: Str, args :: List[Str]) -> [proc] Output {
  match proc.spawn(cmd, args) {
    Ok(r) => {
      ok: r.exit_code == 0,
      exit_code: r.exit_code,
      stdout: r.stdout,
      stderr: r.stderr,
    },
    Err(e) => {
      ok: false,
      exit_code: -1,
      stdout: "",
      stderr: e,
    },
  }
}

# A higher-level pattern: dispatch a *list* of commands and collect
# the outputs. Useful for "fan out a sprint to N agents and gather
# their summaries". Each entry is `(cmd, args)`.
fn dispatch_all(jobs :: List[Tuple[Str, List[Str]]]) -> [proc] List[Output] {
  list.map(jobs, fn (j :: Tuple[Str, List[Str]]) -> [proc] Output {
    run(tuple.fst(j), tuple.snd(j))
  })
}

import "std.tuple" as tuple
