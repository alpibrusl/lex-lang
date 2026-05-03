# Stage 3: judge whether the agent's output satisfies the task. The
# function has *no* declared effects — it's pure, by signature. You
# can run this on a hostile output and the worst it can do is
# return `false`. The type checker will reject any future edit that
# tries to log, write to disk, or call out to the network from
# inside the verifier.

import "../types" as t
import "std.str"  as str

fn run(task :: t.Task, output :: Str) -> Bool {
  str.contains(output, task.criteria)
}
