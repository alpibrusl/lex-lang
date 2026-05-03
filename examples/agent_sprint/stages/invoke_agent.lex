# Stage 2: dispatch the prompt to a sub-agent (claude-code,
# cursor-cli, gemini-cli, ... or `echo` for the demo). The only
# effect is `[proc]`. This stage cannot read the filesystem, cannot
# reach the network, cannot persist anything. The runtime rejects
# any binary not on `--allow-proc` even when `[proc]` is granted, so
# a prompt-injected agent that emits `process.run("rm", ["-rf",
# "/"])` fails at the policy gate, not in user-space.

import "../types"     as t
import "std.process"  as process

fn run(agent_cmd :: Str, task :: t.Task) -> [proc] Result[Str, Str] {
  match process.run(agent_cmd, [task.prompt]) {
    Ok(o)  => Ok(o.stdout),
    Err(e) => Err(e),
  }
}
